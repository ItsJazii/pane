//! Live model pricing — a port of the Mac app's ModelPricingStore.
//!
//! Three sources, most-authoritative first at lookup time:
//!   1. Robin's pricing supplement (Cursor-native models, fast multipliers,
//!      alias regexes mapping log slugs to canonical keys) — updates land
//!      without an app release.
//!   2. LiteLLM's model_prices catalog (USD per token — converted here).
//!   3. models.dev (USD per million; exact-match only — fuzzy-matching a
//!      reseller rate would fabricate dollars).
//!
//! Each source is cached at %APPDATA%\Pane\pricing\ and refreshed at
//! most every 24h (30-minute retry after a failure) with ETag revalidation.
//! `lookup()` never touches the network; `ensure_fresh()` runs on the spend
//! engine's blocking thread. The old hardcoded prices in spend.rs remain
//! the last-resort fallback when a model is missing everywhere.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::providers;

#[derive(Clone, Copy, Debug)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

const SOURCES: [(&str, &str); 3] = [
    (
        "litellm",
        "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json",
    ),
    ("modelsdev", "https://models.dev/api.json"),
    (
        "supplement",
        "https://robinebers.github.io/openusage/pricing_supplement.json",
    ),
];
const REFRESH_MS: i64 = 24 * 3600 * 1000;
const RETRY_MS: i64 = 30 * 60 * 1000;

#[derive(Default)]
struct Store {
    litellm: HashMap<String, Price>,
    modelsdev: HashMap<String, Price>,
    supplement: HashMap<String, Price>,
    fast_multipliers: HashMap<String, f64>,
    alias_rules: Vec<(regex::Regex, String)>,
    memo: HashMap<String, Option<Price>>,
    loaded_from_disk: bool,
}

fn store() -> &'static Mutex<Store> {
    static S: OnceLock<Mutex<Store>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// Set when the spend engine met models no catalog prices — sources are
/// then retried hourly instead of daily, so a newly shipped model (e.g. a
/// fresh Cursor slug) prices as soon as the supplement learns it.
static UNPRICED_HINT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Bumped whenever a source ingests a new document; spend's per-file cache
/// keys on it so already-parsed logs re-price under the new catalog.
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn note_unpriced() {
    UNPRICED_HINT.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub fn generation() -> u64 {
    GENERATION.load(std::sync::atomic::Ordering::Relaxed)
}

fn dir() -> PathBuf {
    providers::config_dir().join("pricing")
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// ---------------------------------------------------------------------------
// Parsing (each source's shape → HashMap<model, Price> per million tokens)
// ---------------------------------------------------------------------------

fn parse_litellm(doc: &Value) -> HashMap<String, Price> {
    let mut out = HashMap::new();
    let Some(obj) = doc.as_object() else { return out };
    for (model, entry) in obj {
        let per_tok = |key: &str| entry.get(key).and_then(Value::as_f64);
        let (Some(input), Some(output)) =
            (per_tok("input_cost_per_token"), per_tok("output_cost_per_token"))
        else {
            continue;
        };
        out.insert(
            model.clone(),
            Price {
                input: input * 1e6,
                output: output * 1e6,
                cache_read: per_tok("cache_read_input_token_cost").map(|v| v * 1e6).unwrap_or(input * 1e6),
                cache_write: per_tok("cache_creation_input_token_cost")
                    .map(|v| v * 1e6)
                    .unwrap_or(input * 1e6),
            },
        );
    }
    out
}

fn parse_modelsdev(doc: &Value) -> HashMap<String, Price> {
    let mut out = HashMap::new();
    let Some(providers) = doc.as_object() else { return out };
    for provider in providers.values() {
        let Some(models) = provider.get("models").and_then(Value::as_object) else { continue };
        for (id, m) in models {
            let Some(cost) = m.get("cost") else { continue };
            let get = |key: &str| cost.get(key).and_then(Value::as_f64);
            let (Some(input), Some(output)) = (get("input"), get("output")) else { continue };
            // First provider wins; models.dev repeats ids across resellers.
            out.entry(id.clone()).or_insert(Price {
                input,
                output,
                cache_read: get("cache_read").unwrap_or(input),
                cache_write: get("cache_write").unwrap_or(input),
            });
        }
    }
    out
}

fn apply_supplement(store: &mut Store, doc: &Value) {
    store.supplement.clear();
    store.fast_multipliers.clear();
    store.alias_rules.clear();
    if let Some(pricing) = doc.get("pricing").and_then(Value::as_object) {
        for (model, entry) in pricing {
            let get = |key: &str| entry.get(key).and_then(Value::as_f64);
            let (Some(input), Some(output)) =
                (get("input_per_million"), get("output_per_million"))
            else {
                continue;
            };
            store.supplement.insert(
                model.clone(),
                Price {
                    input,
                    output,
                    cache_read: get("cache_read_per_million").unwrap_or(input),
                    cache_write: get("cache_write_per_million").unwrap_or(input),
                },
            );
        }
    }
    if let Some(mults) = doc.get("fast_multipliers").and_then(Value::as_object) {
        for (model, v) in mults {
            if let Some(m) = v.as_f64() {
                store.fast_multipliers.insert(model.clone(), m);
            }
        }
    }
    // The supplement is fetched from a third-party URL, so cap what it can
    // feed us: at most 64 alias rules of at most 256 chars each, compiled
    // with a bounded size. (Rust's regex engine is linear-time by design,
    // so ReDoS-style backtracking blowups aren't possible; the caps bound
    // memory and compile cost.)
    if let Some(rules) = doc.get("alias_rules").and_then(Value::as_array) {
        for rule in rules.iter().take(64) {
            let (Some(pattern), Some(canonical)) = (
                rule.get("pattern").and_then(Value::as_str),
                rule.get("canonical").and_then(Value::as_str),
            ) else {
                continue;
            };
            if pattern.len() > 256 || canonical.len() > 128 {
                continue;
            }
            if let Ok(re) = regex::RegexBuilder::new(pattern)
                .size_limit(1 << 20)
                .build()
            {
                store.alias_rules.push((re, canonical.to_string()));
            }
        }
    }
}

fn ingest(store: &mut Store, source: &str, doc: &Value) {
    match source {
        "litellm" => store.litellm = parse_litellm(doc),
        "modelsdev" => store.modelsdev = parse_modelsdev(doc),
        "supplement" => apply_supplement(store, doc),
        _ => {}
    }
    store.memo.clear();
}

// ---------------------------------------------------------------------------
// Disk cache + refresh
// ---------------------------------------------------------------------------

fn load_state() -> Value {
    std::fs::read_to_string(dir().join("state.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| json!({}))
}

fn save_state(state: &Value) {
    let _ = std::fs::create_dir_all(dir());
    let _ = std::fs::write(dir().join("state.json"), state.to_string());
}

fn load_from_disk(store: &mut Store) {
    for (source, _) in SOURCES {
        if let Ok(raw) = std::fs::read_to_string(dir().join(format!("{source}.json"))) {
            if let Ok(doc) = serde_json::from_str::<Value>(&raw) {
                ingest(store, source, &doc);
            }
        }
    }
    store.loaded_from_disk = true;
}

/// Refreshes stale sources (blocking; called from the spend engine's
/// blocking thread). Network failures leave the cached/parsed data in place.
pub fn ensure_fresh() {
    {
        let mut s = store().lock().unwrap();
        if !s.loaded_from_disk {
            load_from_disk(&mut s);
        }
    }

    let mut state = load_state();
    let now = now_ms();
    let refresh_ms = if UNPRICED_HINT.load(std::sync::atomic::Ordering::Relaxed) {
        3_600_000 // unpriced models seen: look for catalog updates hourly
    } else {
        REFRESH_MS
    };
    for (source, url) in SOURCES {
        let entry = state.get(source).cloned().unwrap_or_else(|| json!({}));
        let fetched_at = entry.get("fetchedAt").and_then(Value::as_i64).unwrap_or(0);
        let failed_at = entry.get("failedAt").and_then(Value::as_i64).unwrap_or(0);
        let due = now - fetched_at > refresh_ms && now - failed_at > RETRY_MS;
        if !due {
            continue;
        }
        let etag = entry.get("etag").and_then(Value::as_str).unwrap_or("").to_string();

        let result = tauri::async_runtime::block_on(async {
            let mut req = providers::http().get(url);
            if !etag.is_empty() {
                req = req.header("If-None-Match", etag.clone());
            }
            let resp = req.send().await.map_err(|e| e.to_string())?;
            let status = resp.status().as_u16();
            if status == 304 {
                return Ok((304u16, String::new(), String::new()));
            }
            if !(200..300).contains(&status) {
                return Err(format!("HTTP {status}"));
            }
            let new_etag = resp
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            Ok((status, body, new_etag))
        });

        match result {
            Ok((304, _, _)) => {
                state[source] = json!({ "etag": etag, "fetchedAt": now, "failedAt": 0 });
            }
            Ok((_, body, new_etag)) => match serde_json::from_str::<Value>(&body) {
                Ok(doc) => {
                    let _ = std::fs::create_dir_all(dir());
                    let _ = std::fs::write(dir().join(format!("{source}.json")), &body);
                    ingest(&mut store().lock().unwrap(), source, &doc);
                    state[source] = json!({ "etag": new_etag, "fetchedAt": now, "failedAt": 0 });
                    GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    UNPRICED_HINT.store(false, std::sync::atomic::Ordering::Relaxed);
                    eprintln!("[pane] pricing: refreshed {source}");
                }
                Err(e) => {
                    eprintln!("[pane] pricing: {source} parse failed ({e})");
                    state[source] = json!({ "etag": etag, "fetchedAt": fetched_at, "failedAt": now });
                }
            },
            Err(e) => {
                eprintln!("[pane] pricing: {source} fetch failed ({e})");
                state[source] = json!({ "etag": etag, "fetchedAt": fetched_at, "failedAt": now });
            }
        }
    }
    save_state(&state);
}

// ---------------------------------------------------------------------------
// Lookup
// ---------------------------------------------------------------------------

/// USD per million tokens for `model`, or None if no source prices it.
/// Memoized; disk-cache only (call ensure_fresh() beforehand to update).
pub fn lookup(model: &str) -> Option<Price> {
    let mut s = store().lock().unwrap();
    if !s.loaded_from_disk {
        load_from_disk(&mut s);
    }
    if let Some(hit) = s.memo.get(model) {
        return *hit;
    }
    let result = resolve(&s, model, 0);
    s.memo.insert(model.to_string(), result);
    result
}

fn resolve(s: &Store, model: &str, depth: u8) -> Option<Price> {
    // Alias rules come from a third-party URL; a crafted rule set could
    // otherwise bounce a name between an alias and the -max strip below
    // forever ("foo" → "foo-max" → "foo" → …) and overflow the stack.
    if depth >= 4 {
        return None;
    }
    let canonical = s
        .alias_rules
        .iter()
        .find(|(re, _)| re.is_match(model))
        .map(|(_, c)| c.clone())
        .unwrap_or_else(|| model.to_string());

    if let Some(p) = s.supplement.get(&canonical) {
        return Some(*p);
    }
    if let Some(p) = s.litellm.get(&canonical) {
        return Some(*p);
    }
    // Fast tier: base price × the supplement's multiplier (default 2).
    if let Some(base) = canonical.strip_suffix("-fast") {
        if let Some(p) = s.litellm.get(base).or_else(|| s.supplement.get(base)) {
            let m = s.fast_multipliers.get(base).copied().unwrap_or(2.0);
            return Some(Price {
                input: p.input * m,
                output: p.output * m,
                cache_read: p.cache_read * m,
                cache_write: p.cache_write * m,
            });
        }
    }
    // LiteLLM fuzzy: provider-prefixed keys like "anthropic/claude-…".
    // Prefer an exact segment match; never fuzzy-match models.dev.
    if let Some(p) = s
        .litellm
        .iter()
        .find(|(k, _)| k.rsplit('/').next() == Some(canonical.as_str()))
        .map(|(_, p)| *p)
    {
        return Some(p);
    }
    if let Some(p) = s.modelsdev.get(&canonical) {
        return Some(*p);
    }
    // Cursor's Max-mode slugs ("gpt-5.6-sol-max") bill token-based at the
    // base model's rates, and no catalog carries the -max name itself. Only
    // when the whole chain above misses does the suffix get stripped and
    // the base model resolved — a real -max entry in any source wins.
    canonical
        .strip_suffix("-max")
        .and_then(|base| resolve(s, base, depth + 1))
}

#[cfg(test)]
mod tests {
    /// Live probe: fetches the three catalogs and resolves a few real slugs.
    /// Run via `cargo test --lib pricing -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_probe() {
        super::ensure_fresh();
        for model in [
            "claude-opus-4-8",
            "gpt-5.1-codex-max-xhigh",
            "composer-2.5",
            "claude-4.5-haiku-thinking",
            "gpt-5",
            "some-unknown-model-xyz",
        ] {
            match super::lookup(model) {
                Some(p) => eprintln!(
                    "{model}: in=${:.2} out=${:.2} cr=${:.3} cw=${:.2} (per 1M)",
                    p.input, p.output, p.cache_read, p.cache_write
                ),
                None => eprintln!("{model}: UNPRICED"),
            }
        }
    }
}
