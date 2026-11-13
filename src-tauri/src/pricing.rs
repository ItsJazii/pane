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

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    /// Rates for requests whose prompt crosses 200k tokens — 1M-context
    /// models bill the whole request at a higher tier. None = no tier.
    pub input_200k: Option<f64>,
    pub output_200k: Option<f64>,
    pub cache_read_200k: Option<f64>,
    pub cache_write_200k: Option<f64>,
    /// Explicit 1-hour cache-write rates; absent, 1h writes bill at
    /// twice the (tier-selected) input rate.
    pub cache_write_1h: Option<f64>,
    pub cache_write_1h_200k: Option<f64>,
}

impl Price {
    /// A price with no long-context tier and no explicit 1h rate — what
    /// models.dev, the supplement, and the static fallbacks provide.
    pub fn flat(input: f64, output: f64, cache_read: f64, cache_write: f64) -> Self {
        Price {
            input,
            output,
            cache_read,
            cache_write,
            input_200k: None,
            output_200k: None,
            cache_read_200k: None,
            cache_write_200k: None,
            cache_write_1h: None,
            cache_write_1h_200k: None,
        }
    }
}

/// One request's token counts, cache writes split by lifetime.
pub struct Usage {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
}

/// Dollar cost of one request. Vendors bill the *whole* request (output
/// included) at the >200k tier once the prompt — everything except output —
/// crosses 200k tokens; aggregated sources (Cursor's CSV) opt out because
/// their rows don't preserve request boundaries. 1-hour cache writes bill
/// at twice the tier-selected input rate unless the catalog carries an
/// explicit rate.
pub fn request_cost(p: &Price, u: &Usage, apply_long_context: bool) -> f64 {
    request_cost_at(p, u, if apply_long_context { 200_000.0 } else { f64::INFINITY })
}

/// Like `request_cost`, with an explicit long-context threshold — the tier
/// boundary is vendor-specific (Anthropic switches at 200k prompt tokens,
/// OpenAI's Codex models at 272k).
pub fn request_cost_at(p: &Price, u: &Usage, threshold: f64) -> f64 {
    let prompt = u.input + u.cache_read + u.cache_write_5m + u.cache_write_1h;
    let long = prompt > threshold;
    let pick = |base: f64, above: Option<f64>| if long { above.unwrap_or(base) } else { base };
    let input = pick(p.input, p.input_200k);
    let w1h = if long {
        p.cache_write_1h_200k
            .or(p.input_200k.map(|i| i * 2.0))
            .unwrap_or_else(|| p.cache_write_1h.unwrap_or(p.input * 2.0))
    } else {
        p.cache_write_1h.unwrap_or(p.input * 2.0)
    };
    (u.input * input
        + u.output * pick(p.output, p.output_200k)
        + u.cache_read * pick(p.cache_read, p.cache_read_200k)
        + u.cache_write_5m * pick(p.cache_write, p.cache_write_200k)
        + u.cache_write_1h * w1h)
        / 1e6
}

/// 2026-09-10T04:00Z — DeepSeek's V4.1 Flash card changeover: before it
/// the model billed at AihubMix's flat launch card, and no peak windows
/// existed at all.
pub const V41_FLASH_CHANGEOVER_MS: i64 = 1_789_012_800_000;

/// The V4.1 Flash SKU under any log spelling: gateway prefixes and Pi's
/// routing tag peeled, dated snapshot tails and effort suffixes stripped
/// (the same trims resolve() does, so every name that prices at this
/// card also peaks at it).
fn v41_flash_slug(model: &str) -> bool {
    let mut bare = model.rsplit(['/', '\u{1}']).next().unwrap_or(model);
    for suf in ["-xhigh", "-light", "-low", "-medium", "-high", "-max", "-ultra"] {
        if let Some(next) = bare.strip_suffix(suf) {
            bare = next;
        }
    }
    if let Some((head, tail)) = bare.rsplit_once('-') {
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
            bare = head;
        }
    }
    matches!(bare, "deepseek-v4.1-flash" | "deepseek-v4-1-flash")
}

/// DeepSeek V4.1 Flash peak hours bill at 2× the whole card (official
/// schedule effective 2026-09-10, mirrored by AihubMix): 01:00–04:00 and
/// 06:00–10:00 UTC on weekdays; weekends are always off-peak. (Public
/// holidays are off-peak too, but we can't detect those — a handful of
/// days a year read as peak.) Every other model: 1.0.
pub fn peak_multiplier(model: &str, ts_ms: i64) -> f64 {
    use chrono::{Datelike, Timelike};
    if ts_ms < V41_FLASH_CHANGEOVER_MS || !v41_flash_slug(model) {
        return 1.0;
    }
    let Some(ts) = chrono::DateTime::from_timestamp_millis(ts_ms) else { return 1.0 };
    if ts.weekday().num_days_from_monday() > 4 {
        return 1.0; // weekend
    }
    let mins = ts.num_seconds_from_midnight() / 60;
    if (60..240).contains(&mins) || (360..600).contains(&mins) { 2.0 } else { 1.0 }
}

/// Whether peak_multiplier can ever return 2× for this model.
pub fn peak_windowed(model: &str) -> bool {
    v41_flash_slug(model)
}

/// The pre-changeover card (AihubMix's flat launch pricing) for V4.1
/// Flash events before 2026-09-10T04:00Z. None for other models or
/// later events — those use the resolved card as-is.
pub fn v41_flash_legacy_card(model: &str, ts_ms: i64) -> Option<Price> {
    if ts_ms >= V41_FLASH_CHANGEOVER_MS || !v41_flash_slug(model) {
        return None;
    }
    Some(Price::flat(0.142, 0.284, 0.0284, 0.142))
}

/// 2026-12-01T08:00Z — end of Nov 30 Pacific. Google dates the Gemini 4
/// Argon AND Carbon intro promos in PT (ai.google.dev/gemini-api/docs/
/// latest-model): $2 in / $10 out / $0.10 cache read through 2026-11-30,
/// then the standard $4/$20/$0.20 card gemini4_list_card enforces over the
/// catalogs.
pub const GEMINI4_INTRO_END_MS: i64 = 1_796_112_000_000;

/// The last `/` segment when it spells Gemini 4 Argon — exactly or as
/// `gemini-4-argon-<suffix>` (effort tails, -fast, dated snapshots),
/// case-insensitive. Route-agnostic: AIHubMix spellings still match.
fn argon_segment(model: &str) -> Option<&str> {
    gemini4_segment(model).filter(|seg| seg.to_lowercase().starts_with("gemini-4-argon"))
}

/// The last `/` segment when it spells Gemini 4 Argon or Carbon — exactly
/// or as `gemini-4-(argon|carbon)-<suffix>` (effort tails, -fast, dated
/// snapshots), case-insensitive. Route-agnostic: AIHubMix spellings still
/// match.
fn gemini4_segment(model: &str) -> Option<&str> {
    let seg = model.rsplit('/').next().unwrap_or(model);
    let lower = seg.to_lowercase();
    (lower == "gemini-4-argon"
        || lower.starts_with("gemini-4-argon-")
        || lower == "gemini-4-carbon"
        || lower.starts_with("gemini-4-carbon-"))
    .then_some(seg)
}

/// Whether `model` takes Google's dated Gemini 4 card. AIHubMix bills
/// Argon on its own flat card year-round, so the aihubmix/ Argon spelling
/// is excluded; AIHubMix lists Carbon at Google's own rate
/// (aihubmix.com/model/gemini-4-carbon: $2 / $10 / $0.10 cache read), so
/// aihubmix/ Carbon follows the Google card on both sides of the cutoff.
fn gemini4_google_slug(model: &str) -> bool {
    if model.to_lowercase().starts_with("aihubmix/") && argon_segment(model).is_some() {
        return false;
    }
    gemini4_segment(model).is_some()
}

/// `card` scoped to Google's Gemini 4 Argon/Carbon slugs — None for other
/// models and the AIHubMix Argon spelling (see gemini4_google_slug). A
/// `-fast` slug scales the card by the supplement's fast multiplier —
/// same as resolve()'s tier pricing.
fn gemini4_google_card_in(s: &Store, model: &str, card: Price) -> Option<Price> {
    if !gemini4_google_slug(model) {
        return None;
    }
    let lower = model.to_lowercase();
    let seg = lower.rsplit('/').next().unwrap_or(lower.as_str());
    if !seg.ends_with("-fast") {
        return Some(card);
    }
    // Same base resolve() uses: the full slug minus the -fast tail.
    let base = lower.strip_suffix("-fast").unwrap_or(seg);
    Some(scaled_price(&card, fast_tier_mult(s, base)))
}

/// Google's intro card for Gemini 4 Argon and Carbon before
/// GEMINI4_INTRO_END_MS:
/// $2 in / $10 out / $0.10 cache read; cache write bills at intro input
/// ($2) since no cache-write rate is published.
fn gemini4_intro_card_in(s: &Store, model: &str, ts_ms: i64) -> Option<Price> {
    if ts_ms >= GEMINI4_INTRO_END_MS {
        return None;
    }
    // The `cyber-permissive` SKUs were never on the intro promo — always
    // the list card (which still applies to them after the cutoff).
    if cyber_permissive_slug(model) {
        return None;
    }
    gemini4_google_card_in(s, model, Price::flat(2.0, 10.0, 0.10, 2.0))
}

/// Gemini 4 Argon/Carbon `-cyber-permissive` spellings — excluded from
/// the intro promo only.
fn cyber_permissive_slug(model: &str) -> bool {
    let seg = model.rsplit('/').next().unwrap_or(model).to_lowercase();
    seg.contains("cyber-permissive")
}

pub fn gemini4_intro_card(model: &str, ts_ms: i64) -> Option<Price> {
    gemini4_intro_card_in(&store().lock().unwrap(), model, ts_ms)
}

/// Google's list card for Gemini 4 Argon and Carbon from
/// GEMINI4_INTRO_END_MS on:
/// $4 in / $20 out / $0.20 cache read, cache write at input. Enforced
/// over the catalogs, which still carry the intro rate after the promo
/// ends — the bare baked arm alone would lose to LiteLLM's stale card.
pub fn gemini4_list_card(model: &str, ts_ms: i64) -> Option<Price> {
    if ts_ms < GEMINI4_INTRO_END_MS {
        return None;
    }
    gemini4_google_card_in(&store().lock().unwrap(), model, Price::flat(4.0, 20.0, 0.20, 4.0))
}

/// Every effective-dated card: a resolved price that differs by date.
/// cost_for and the Hermes session splitter consult this before the
/// resolved card — a bare result is the card in force at ts_ms. Argon
/// and Carbon are covered on both sides of the promo: intro before the
/// cutoff, the enforced list card after it.
pub fn dated_card(model: &str, ts_ms: i64) -> Option<Price> {
    // Scope the lock — argon_* re-lock the store themselves.
    let feed = feed_dated_card(&store().lock().unwrap(), model, ts_ms);
    feed
        .or_else(|| v41_flash_legacy_card(model, ts_ms))
        .or_else(|| gemini4_intro_card(model, ts_ms))
        .or_else(|| gemini4_list_card(model, ts_ms))
}

/// A `dated_pricing` feed entry whose window contains `ts_ms` — first
/// match wins. Slugs normalize like the fast path: last segment
/// lowercased, `-fast` peeled (card scales by fast_tier_mult), then
/// effort/mode tails until the entry's model equals what's left.
/// `exclude_prefixes` keeps gateway spellings on their own card.
fn feed_dated_card(s: &Store, model: &str, ts_ms: i64) -> Option<Price> {
    let (entries, fast) = feed_dated_entries(s, model);
    let e = entries
        .into_iter()
        .find(|e| !(e.from.is_some_and(|f| ts_ms < f) || e.until.is_some_and(|u| ts_ms >= u)))?;
    Some(match &fast {
        Some(base) => scaled_price(&e.price, fast_tier_mult(s, base)),
        None => e.price,
    })
}

/// The `dated_pricing` entries that can apply to `model` (any window),
/// in feed order, plus the `-fast` base when the slug is a fast tier.
fn feed_dated_entries<'a>(s: &'a Store, model: &str) -> (Vec<&'a DatedEntry>, Option<String>) {
    let lower = model.to_lowercase();
    let mut stem = lower.rsplit('/').next().unwrap_or(lower.as_str()).to_string();
    let fast = stem.strip_suffix("-fast").map(|b| b.to_string());
    if let Some(b) = &fast {
        stem = b.clone();
    }
    while let Some(next) = ["-xhigh", "-light", "-low", "-medium", "-high", "-max", "-ultra"]
        .iter()
        .find_map(|suf| stem.strip_suffix(suf))
    {
        stem = next.to_string();
    }
    let entries = s
        .dated_pricing
        .iter()
        .filter(|e| e.model == stem && !e.exclude_prefixes.iter().any(|p| lower.starts_with(p.as_str())))
        .collect();
    (entries, fast)
}

/// Everything the feed's `dated_pricing` could answer for `model`, as a
/// stable string — empty when no entry applies. The spend cache records
/// it per model (cost_for applies dated cards after the lookup probes),
/// so a feed edit that only touches `dated_pricing` still re-prices the
/// cached history it affects.
pub fn dated_feed_fingerprint(model: &str) -> String {
    let mut s = store().lock().unwrap();
    if !s.loaded_from_disk {
        load_from_disk(&mut s);
    }
    dated_feed_fingerprint_in(&s, model)
}

fn dated_feed_fingerprint_in(s: &Store, model: &str) -> String {
    let (entries, fast) = feed_dated_entries(s, model);
    let mut out: String = entries
        .iter()
        .map(|e| format!("{:?}|{:?}|{:?};", e.from, e.until, e.price))
        .collect();
    if let (Some(base), false) = (&fast, out.is_empty()) {
        out.push_str(&format!("fast={}", fast_tier_mult(s, base)));
    }
    out
}

/// Only two `hy4` spellings take the AIHubMix card: the bare slug
/// (`hy4`/`HY4` — how Claude Code logs it on AIHubMix's Anthropic
/// endpoint) and an explicit `aihubmix` prefix (`aihubmix/hy4`,
/// `aihubmix\u{1}hy4`). `tencent/hy4` is Tencent's own endpoint and
/// stays on its provider card.
pub fn aihubmix_hy4_route(model: &str) -> bool {
    if model.eq_ignore_ascii_case("hy4") {
        return true;
    }
    if let Some(idx) = model.rfind(['/', '\u{1}']) {
        return model[..idx].eq_ignore_ascii_case("aihubmix")
            && model[idx + 1..].eq_ignore_ascii_case("hy4");
    }
    false
}

/// The AIHubMix spelling of a model that only reaches the caller through
/// the gateway — callers have already established the route is AIHubMix.
/// Argon slugs normalise as before (`google/gemini-4-argon`, bare).
/// `deepseek-flash` is AIHubMix's alias for deepseek-v4.1-flash-1017 on
/// its Anthropic endpoint (same published card), and any V4.1 Flash
/// spelling maps to the gateway slug — the public catalog prices the
/// bare `deepseek-flash` alias flat with no peak logic, which is wrong.
/// Tencent's `hy4` — which Claude Code logs bare on that same endpoint —
/// maps to the `aihubmix/hy4` card (exactly `hy4`; `hy4-preview` is a
/// different SKU the callers route themselves). `coding-qwen4-max-preview`
/// is AIHubMix's own Token Plan SKU name, so it always maps to its card.
/// Everything else is not an AIHubMix route.
pub fn aihubmix_scoped(model: &str) -> Option<String> {
    let seg = model.rsplit(['/', '\u{1}']).next().unwrap_or(model);
    if seg.eq_ignore_ascii_case("deepseek-flash") {
        return Some("aihubmix/deepseek-v4.1-flash".into());
    }
    if seg.eq_ignore_ascii_case("coding-qwen4-max-preview") {
        return Some("aihubmix/coding-qwen4-max-preview".into());
    }
    if aihubmix_hy4_route(model) {
        return Some("aihubmix/hy4".into());
    }
    if v41_flash_slug(model) {
        return Some(format!("aihubmix/{seg}"));
    }
    argon_segment(model).map(|seg| format!("aihubmix/{seg}"))
}

/// The slug to price a row that was billed by AIHubMix — ONLY for
/// callers that already know the row crossed the gateway (the pi/omp
/// and DeepSeek Harness `aihubmix` routes). Everything aihubmix_scoped
/// covers, plus its own M3.1-Flash card: `minimax-m3.1-flash` in any
/// casing resolves to `aihubmix/minimax-m3.1-flash`. Do not use on
/// Claude rows — Claude Code logs the same MiniMax-M3.1-Flash string
/// for MiniMax's own endpoint, which models.dev prices correctly.
pub fn aihubmix_gateway_slug(model: &str) -> String {
    if let Some(s) = aihubmix_scoped(model) {
        return s;
    }
    let seg = model.rsplit(['/', '\u{1}']).next().unwrap_or(model);
    if seg.eq_ignore_ascii_case("minimax-m3.1-flash") {
        return "aihubmix/minimax-m3.1-flash".to_string();
    }
    if seg.eq_ignore_ascii_case("hy4") {
        return "aihubmix/hy4".to_string();
    }
    model.to_string()
}

/// Milliseconds of [start_ms, end_ms) inside V4.1 Flash peak windows —
/// Hermes rows aggregate whole sessions, so a boundary-crossing session
/// splits its cost by this share. The changeover clamps the interval:
/// peak windows didn't exist before it.
pub fn peak_overlap_ms(start_ms: i64, end_ms: i64) -> i64 {
    use chrono::Datelike;
    let start_ms = start_ms.max(V41_FLASH_CHANGEOVER_MS);
    if end_ms <= start_ms {
        return 0;
    }
    let mut total = 0;
    let mut day = start_ms - start_ms.rem_euclid(86_400_000);
    // Sessions don't span weeks; the cap just bounds a corrupt interval.
    for _ in 0..45 {
        if day >= end_ms {
            break;
        }
        let Some(ts) = chrono::DateTime::from_timestamp_millis(day) else { break };
        if ts.weekday().num_days_from_monday() <= 4 {
            for (s, e) in [(60_i64, 240), (360, 600)] {
                let window_start = day + s * 60_000;
                let window_end = day + e * 60_000;
                total += (end_ms.min(window_end) - start_ms.max(window_start)).max(0);
            }
        }
        day += 86_400_000;
    }
    total
}

/// The supplement's fast multiplier for a model, 1.0 when none is
/// published — a fast-flagged request without data bills at standard
/// rates rather than a guessed premium (Mac behavior). Claude Fable/
/// Mythos 5.2's documented fast tier is a flat 2×, so it is the one
/// built-in default until the supplement carries it.
pub fn fast_multiplier(model: &str) -> f64 {
    let s = store().lock().unwrap();
    fast_multiplier_in(&s, model)
}

fn fast_multiplier_in(s: &Store, model: &str) -> f64 {
    if let Some(m) = s.fast_multipliers.get(model) {
        return *m;
    }
    if fable52_base(model).is_some() {
        // The supplement's multiplier for the base (dash or dotted key)
        // wins; otherwise the documented fast tier is a flat 2×.
        return fable52_supplement_mult(s, model).unwrap_or(2.0);
    }
    1.0
}

const SOURCES: [(&str, &str); 5] = [
    (
        "litellm",
        "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json",
    ),
    ("modelsdev", "https://models.dev/api.json"),
    (
        "supplement",
        "https://raw.githubusercontent.com/ItsJazii/pane/main/pricing/supplement.json",
    ),
    ("openrouter", "https://openrouter.ai/api/v1/models"),
    ("aihubmix", "https://aihubmix.com/api/v1/models"),
];
const REFRESH_MS: i64 = 24 * 3600 * 1000;
/// Pane's own pricing feed moves faster than app releases — promos and
/// dated cards ship there by PR, so it re-fetches more often than the
/// third-party catalogs.
const SUPPLEMENT_REFRESH_MS: i64 = 6 * 3600 * 1000;
const RETRY_MS: i64 = 30 * 60 * 1000;

/// How often `source` is re-fetched: hourly while an unpriced model was
/// seen (any source), 6 h for Pane's own feed, daily for the rest.
fn refresh_ms(source: &str, unpriced: bool) -> i64 {
    if unpriced {
        3_600_000
    } else if source == "supplement" {
        SUPPLEMENT_REFRESH_MS
    } else {
        REFRESH_MS
    }
}
/// The catalogs are third-party feeds; a compromised one must not be able
/// to fill memory (and then the disk cache) with an arbitrarily large
/// response. Largest legitimate source today is LiteLLM at ~3 MB — 32 MB
/// leaves room to grow while still bounding the damage.
const MAX_CATALOG_BYTES: usize = 32 * 1024 * 1024;

#[derive(Default)]
struct Store {
    litellm: HashMap<String, Price>,
    modelsdev: HashMap<String, Price>,
    supplement: HashMap<String, Price>,
    /// OpenRouter's live model list, keyed by full lowercase id — plus
    /// last-segment and dot-normalized-segment indexes so `canonical`'s
    /// own spelling finds its listing. Segment entries keep the
    /// lexicographically smallest id so picks are deterministic.
    openrouter: HashMap<String, Price>,
    openrouter_seg: HashMap<String, Price>,
    openrouter_dashseg: HashMap<String, Price>,
    /// AIHubMix's own published model cards, keyed by lowercase model id
    /// and by its last path segment (`inclusionai/ling-1t` → `ling-1t`).
    /// Only `aihubmix/…` slugs consult it — the last resort for a row the
    /// gateway billed that nothing else can price.
    aihubmix: HashMap<String, Price>,
    fast_multipliers: HashMap<String, f64>,
    alias_rules: Vec<(regex::Regex, String)>,
    /// Time-windowed cards from Pane's pricing feed (promos / price
    /// changes that can't wait for an app release).
    dated_pricing: Vec<DatedEntry>,
    memo: HashMap<String, Option<Price>>,
    loaded_from_disk: bool,
}

/// One `dated_pricing` entry from Pane's feed: `model` (plain slug)
/// prices at `price` inside [from, until); either bound may be open.
struct DatedEntry {
    model: String,
    from: Option<i64>,
    until: Option<i64>,
    price: Price,
    exclude_prefixes: Vec<String>,
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

/// Set when a spend scan needed AIHubMix's own catalog (a row the gateway
/// billed that nothing else prices). The catalog is only downloaded for
/// installs that use AIHubMix: a key is configured, or this flag is set.
static AIHUBMIX_WANTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The `aihubmix/<id>` slug that prices `model` on AIHubMix's own
/// published card — the last resort for a row AIHubMix billed when no
/// other source knows the model. Marks the catalog as wanted so the next
/// refresh downloads it. The whole logged id is kept (`vendor/model` →
/// `aihubmix/vendor/model`): resolve() tries the catalog's exact id
/// first and only then its last segment, so two vendors sharing a model
/// name each get their own card. An already-prefixed slug keeps its id.
pub fn aihubmix_catalog_slug(model: &str) -> String {
    AIHUBMIX_WANTED.store(true, std::sync::atomic::Ordering::Relaxed);
    let lower = model.to_lowercase();
    let id = lower
        .strip_prefix("aihubmix/")
        .or_else(|| lower.strip_prefix("aihubmix\u{1}"))
        .unwrap_or(lower.as_str());
    format!("aihubmix/{id}")
}

/// Test hook: list `id` in the shared AIHubMix catalog. Callers use ids
/// no real catalog carries, so parallel tests can't see each other.
#[cfg(test)]
pub fn seed_aihubmix_for_test(id: &str, price: Price) {
    let mut s = store().lock().unwrap();
    if !s.loaded_from_disk {
        load_from_disk(&mut s);
    }
    s.aihubmix.insert(id.to_lowercase(), price);
    s.memo.clear();
}

pub fn generation() -> u64 {
    GENERATION.load(std::sync::atomic::Ordering::Relaxed)
}

/// Bump whenever the baked-in pricing behavior changes (stale-catalog
/// corrections, builtin fallbacks, long-context tables): the persisted
/// spend cache holds pre-priced totals, and only the catalog *files* are
/// fingerprinted below — an app update that reprices the same files would
/// otherwise leave history at the old dollars until upstream happens to
/// rewrite a catalog.
const CORRECTIONS_REV: u32 = 31; // 19: Antigravity `-trial` ids (gemini-4-argon-trial) price like the base card; 20: Argon post-intro list card enforced over catalogs; 21: Claude Code deepseek-flash / V4.1 Flash priced on AIHubMix's card; 22: `deepseek/` spellings take the baked official card over models.dev's same-prefix reseller ids; 23: AIHubMix MiniMax M3.1 Flash card; 24: Moonshot Kimi K3.1 Pro + Highspeed cards; 25: Claude Fable/Mythos 5.2 card + 2× fast mode; 26: AIHubMix Tencent Hy4 card (Claude Code `hy4` rows); 27: Gemini 4 Carbon dated card, Devin Argon at Google's price, OpenRouter source, Pane-owned feed + dated_pricing; 28: AIHubMix Coding Qwen4 Max Preview card; 29: Claude rows with an unpriceable logged model fall back to requestedModel; 30: AIHubMix's own catalog prices gateway-billed rows nothing else knows; 31: SWE-2 free card, SWE-2 Max Lightning promo card

/// The corrections revision on its own — the spend cache treats a changed
/// revision as a hard discard (the *code* that prices changed), while a
/// changed catalog file only re-prices files whose recorded price probes
/// no longer replay identically.
///
/// Bump this whenever baked rates *or* spend.rs's own pricing tables change
/// (`claude_price`, `codex_price`, `grok_price`, `codex_long_context`,
/// `codex_priority_multiplier` hardcoded arms, `codex_no_cache_discount`,
/// `request_cost` thresholds). Probes only witness catalog lookups — they
/// cannot vouch for that code. A forgotten bump used to get papered over
/// within a day by catalog-stamp churn; it now leaves stale dollars until
/// the next bump.
///
/// Parser-logic changes (`claude_line`, `codex_line`, `pi_line`, token
/// field spellings, dedup rules) bump `PERSIST_VERSION` in spend.rs
/// instead — probes cannot see those either.
pub fn corrections_rev() -> u32 {
    CORRECTIONS_REV
}
/// The live OpenUsage supplement is ~105 alias rules (Daybreak, Cursor
/// Router prose names, GPT-5.3–5.6 effort slugs). 64 silently dropped
/// everything after `gpt-5.2`. Per-rule caps below still bound memory.
const MAX_ALIAS_RULES: usize = 256;
/// Bounds for what the supplement feed may claim — it is trusted by URL
/// alone yet outranks both catalogs in resolve(). No real model prices
/// above ~$600/M, and a fast tier outside 1×–10× is a corrupt feed, not
/// a SKU. Entries outside the bounds are dropped, not clamped.
const MAX_SUPPLEMENT_PRICE: f64 = 10_000.0;
const MAX_FAST_MULTIPLIER: f64 = 10.0;

/// Stable fingerprint of the effective pricing inputs: the on-disk catalog
/// files plus this binary's corrections revision. The persistent spend
/// cache stores costs priced under a specific catalog set; when the stamp
/// changes (a refresh rewrote a catalog, or an update shipped changed baked
/// pricing) the cache revalidates each file's recorded price probes and
/// re-parses only the files whose prices actually moved — never serving
/// stale dollars, never re-reading gigabytes because a catalog mtime ticked.
pub fn catalog_stamp() -> String {
    let files = SOURCES
        .iter()
        .map(|(source, _)| {
            let path = dir().join(format!("{source}.json"));
            match std::fs::metadata(&path) {
                Ok(m) => {
                    let ms = m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis())
                        .unwrap_or(0);
                    format!("{source}:{ms}:{}", m.len())
                }
                Err(_) => format!("{source}:absent"),
            }
        })
        .collect::<Vec<_>>()
        .join("|");
    format!("{files}|corrections:{CORRECTIONS_REV}")
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

/// OpenRouter's /api/v1/models — same-day listings for brand-new models.
/// `pricing` rates are per-token decimal strings (numbers accepted too);
/// keyed by lowercase id. Segment indexes keep the smallest id per last
/// path segment (and per dot-to-dash normalized segment) so a model
/// spelled differently upstream still matches deterministically.
/// Skipped: `:variant` ids (`:free`, `:batch`, `:extended`…), openrouter/
/// meta ids, negative rates (`openrouter/auto` is "-1"), 0/0 entries and
/// implausible prices.
fn parse_openrouter(doc: &Value) -> (HashMap<String, Price>, HashMap<String, Price>, HashMap<String, Price>) {
    let mut exact = HashMap::new();
    let mut seg: HashMap<String, (String, Price)> = HashMap::new();
    let mut dashseg: HashMap<String, (String, Price)> = HashMap::new();
    let Some(data) = doc.get("data").and_then(Value::as_array) else {
        return (HashMap::new(), HashMap::new(), HashMap::new());
    };
    let plausible = |v: f64| v.is_finite() && (0.0..=MAX_SUPPLEMENT_PRICE).contains(&v);
    for entry in data {
        let Some(id) = entry.get("id").and_then(Value::as_str) else { continue };
        if id.contains(':') || id.starts_with("openrouter/") {
            continue;
        }
        let Some(pricing) = entry.get("pricing") else { continue };
        let per_tok = |key: &str| {
            let v = pricing.get(key)?;
            v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
        };
        let (Some(input), Some(output)) = (per_tok("prompt"), per_tok("completion")) else {
            continue;
        };
        let cache_read = per_tok("input_cache_read").unwrap_or(input);
        let cache_write = per_tok("input_cache_write").unwrap_or(input);
        let rates = [input, output, cache_read, cache_write];
        if !rates.iter().all(|v| v.is_finite() && *v >= 0.0 && plausible(*v)) {
            continue;
        }
        let price = Price::flat(input * 1e6, output * 1e6, cache_read * 1e6, cache_write * 1e6);
        if price.input <= 0.0 && price.output <= 0.0 {
            continue;
        }
        let key = id.to_lowercase();
        let last = key.rsplit('/').next().unwrap_or(key.as_str()).to_string();
        exact.insert(key.clone(), price);
        // The lexicographically smallest full id wins the segment slot —
        // deterministic no matter how many vendors list the same tail.
        seg.entry(last.clone())
            .and_modify(|(id, p): &mut (String, Price)| {
                if id.as_str() > key.as_str() { *id = key.clone(); *p = price; }
            })
            .or_insert((key.clone(), price));
        dashseg
            .entry(last.replace('.', "-"))
            .and_modify(|(id, p): &mut (String, Price)| {
                if id.as_str() > key.as_str() { *id = key.clone(); *p = price; }
            })
            .or_insert((key, price));
    }
    (
        exact,
        seg.into_iter().map(|(k, (_, p))| (k, p)).collect(),
        dashseg.into_iter().map(|(k, (_, p))| (k, p)).collect(),
    )
}

/// The price-only projection of an OpenRouter catalog — what
/// parse_openrouter actually consumes, sorted by id and serialized
/// deterministically. The live endpoint churns daily in fields pane
/// never reads and sends no ETag; the disk cache stores this form so a
/// refresh with no price move writes identical bytes.
fn compact_openrouter(doc: &Value) -> String {
    let num = |v: Option<&Value>| -> Option<f64> {
        v.and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
        })
    };
    let plausible = |v: f64| v.is_finite() && (0.0..=MAX_SUPPLEMENT_PRICE).contains(&v);
    let mut rows: Vec<Value> = Vec::new();
    for e in doc
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = e.get("id").and_then(Value::as_str) else { continue };
        if id.contains(':') || id.starts_with("openrouter/") {
            continue;
        }
        let Some(pricing) = e.get("pricing") else { continue };
        let (Some(input), Some(output)) = (
            num(pricing.get("prompt")),
            num(pricing.get("completion")),
        ) else {
            continue;
        };
        let cache_read = num(pricing.get("input_cache_read")).unwrap_or(input);
        let cache_write = num(pricing.get("input_cache_write")).unwrap_or(input);
        if !(input >= 0.0 && plausible(input))
            || !(output >= 0.0 && plausible(output))
            || !(cache_read >= 0.0 && plausible(cache_read))
            || !(cache_write >= 0.0 && plausible(cache_write))
            || (input == 0.0 && output == 0.0)
        {
            continue;
        }
        rows.push(json!({"id": id, "pricing": {
            "prompt": input,
            "completion": output,
            "input_cache_read": cache_read,
            "input_cache_write": cache_write,
        }}));
    }
    rows.sort_by(|a, b| {
        a["id"].as_str().unwrap_or("").cmp(b["id"].as_str().unwrap_or(""))
    });
    serde_json::to_string(&json!({"data": rows})).unwrap_or_default()
}

/// The text-model cards in AIHubMix's public catalog
/// (aihubmix.com/api/v1/models), sorted by id: (model id, price). Rates
/// are already USD per million tokens; an unpublished cache rate bills
/// at input, like the baked AIHubMix arms. Skipped: non-text SKUs (image,
/// video, embedding… — their `input`/`output` are other units), the
/// `auto` router, and anything missing or outside the plausible range.
/// The compact disk form carries no `types`, so an absent field passes.
fn aihubmix_rows(doc: &Value) -> Vec<(String, Price)> {
    let num = |v: Option<&Value>| -> Option<f64> {
        v.and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok())))
    };
    let plausible = |v: f64| v.is_finite() && (0.0..=MAX_SUPPLEMENT_PRICE).contains(&v);
    let mut rows: Vec<(String, Price)> = Vec::new();
    for e in doc.get("data").and_then(Value::as_array).into_iter().flatten() {
        let Some(id) = e.get("model_id").and_then(Value::as_str) else { continue };
        if id.is_empty() || id.len() > 128 || id.eq_ignore_ascii_case("auto") {
            continue;
        }
        if let Some(types) = e.get("types").and_then(Value::as_str) {
            if !types.split(',').any(|t| t.trim() == "llm") {
                continue;
            }
        }
        let Some(pricing) = e.get("pricing") else { continue };
        let (Some(input), Some(output)) =
            (num(pricing.get("input")), num(pricing.get("output")))
        else {
            continue;
        };
        let cache_read = num(pricing.get("cache_read")).unwrap_or(input);
        let cache_write = num(pricing.get("cache_write")).unwrap_or(input);
        if ![input, output, cache_read, cache_write].into_iter().all(plausible) {
            continue;
        }
        rows.push((id.to_string(), Price::flat(input, output, cache_read, cache_write)));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

/// AIHubMix's catalog as a lookup map: every lowercase model id, plus its
/// last path segment where no full id already claims that key. Rows
/// arrive sorted, so the smallest id wins any collision deterministically.
fn parse_aihubmix(doc: &Value) -> HashMap<String, Price> {
    let rows = aihubmix_rows(doc);
    let mut out: HashMap<String, Price> = HashMap::new();
    for (id, price) in &rows {
        out.entry(id.to_lowercase()).or_insert(*price);
    }
    for (id, price) in &rows {
        let lower = id.to_lowercase();
        if let Some((_, seg)) = lower.rsplit_once('/') {
            out.entry(seg.to_string()).or_insert(*price);
        }
    }
    out
}

/// The price-only projection of AIHubMix's catalog — what parse_aihubmix
/// consumes, sorted and serialized deterministically (~900 KB of
/// descriptions and capability flags shrink to the cards). A refresh
/// with no price move writes identical bytes, like compact_openrouter.
fn compact_aihubmix(doc: &Value) -> String {
    let rows: Vec<Value> = aihubmix_rows(doc)
        .into_iter()
        .map(|(id, p)| {
            json!({"model_id": id, "pricing": {
                "input": p.input,
                "output": p.output,
                "cache_read": p.cache_read,
                "cache_write": p.cache_write,
            }})
        })
        .collect();
    serde_json::to_string(&json!({"data": rows})).unwrap_or_default()
}

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
        let per_m = |key: &str| per_tok(key).map(|v| v * 1e6);
        out.insert(
            model.clone(),
            Price {
                input: input * 1e6,
                output: output * 1e6,
                cache_read: per_m("cache_read_input_token_cost").unwrap_or(input * 1e6),
                cache_write: per_m("cache_creation_input_token_cost").unwrap_or(input * 1e6),
                input_200k: per_m("input_cost_per_token_above_200k_tokens"),
                output_200k: per_m("output_cost_per_token_above_200k_tokens"),
                cache_read_200k: per_m("cache_read_input_token_cost_above_200k_tokens"),
                cache_write_200k: per_m("cache_creation_input_token_cost_above_200k_tokens"),
                cache_write_1h: per_m("cache_creation_input_token_cost_above_1hr"),
                cache_write_1h_200k: per_m("cache_creation_input_token_cost_above_1hr_above_200k_tokens"),
            },
        );
    }
    out
}

fn parse_modelsdev(doc: &Value) -> HashMap<String, Price> {
    // models.dev repeats ids across resellers with varying completeness —
    // the entry documenting the most cache fields wins (ties: first seen),
    // so a reseller stub with no cache rates can't default a $0.30 cache
    // hit to the $3.00 input price.
    let mut out: HashMap<String, (Price, u8)> = HashMap::new();
    let Some(providers) = doc.as_object() else { return HashMap::new() };
    for provider in providers.values() {
        let Some(models) = provider.get("models").and_then(Value::as_object) else { continue };
        for (id, m) in models {
            let Some(cost) = m.get("cost") else { continue };
            let get = |key: &str| cost.get(key).and_then(Value::as_f64);
            let (Some(input), Some(output)) = (get("input"), get("output")) else { continue };
            let score =
                get("cache_read").is_some() as u8 + get("cache_write").is_some() as u8;
            let price = Price::flat(
                input,
                output,
                get("cache_read").unwrap_or(input),
                get("cache_write").unwrap_or(input),
            );
            match out.entry(id.clone()) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert((price, score));
                }
                std::collections::hash_map::Entry::Occupied(mut e) if score > e.get().1 => {
                    e.insert((price, score));
                }
                _ => {}
            }
        }
    }
    out.into_iter().map(|(k, (p, _))| (k, p)).collect()
}

/// One feed `pricing`/`dated_pricing` price object: required
/// input/output per-million plus optional cache and tier fields.
/// `Ok(None)` — no input/output (a `pricing` entry to skip); `Err` —
/// anything out of range (the entry must be dropped and counted).
fn feed_card(entry: &Value) -> Result<Option<Price>, ()> {
    let get = |key: &str| entry.get(key).and_then(Value::as_f64);
    let (Some(input), Some(output)) =
        (get("input_per_million"), get("output_per_million"))
    else {
        return Ok(None);
    };
    let cache_read = get("cache_read_per_million").unwrap_or(input);
    let cache_write = get("cache_write_per_million").unwrap_or(input);
    let plausible = |v: f64| v.is_finite() && (0.0..=MAX_SUPPLEMENT_PRICE).contains(&v);
    // Optional tier fields let the feed carry a full card — an
    // out-of-range optional value rejects the whole entry.
    let opt = |key: &str| -> Result<Option<f64>, ()> {
        match get(key) {
            Some(v) if plausible(v) => Ok(Some(v)),
            Some(_) => Err(()),
            None => Ok(None),
        }
    };
    let cache_write_1h = opt("cache_write_1h_per_million")?;
    let input_200k = opt("input_above_200k_per_million")?;
    let output_200k = opt("output_above_200k_per_million")?;
    let cache_read_200k = opt("cache_read_above_200k_per_million")?;
    let cache_write_200k = opt("cache_write_above_200k_per_million")?;
    if ![input, output, cache_read, cache_write].into_iter().all(plausible) {
        return Err(());
    }
    let mut p = Price::flat(input, output, cache_read, cache_write);
    p.cache_write_1h = cache_write_1h;
    p.input_200k = input_200k;
    p.output_200k = output_200k;
    p.cache_read_200k = cache_read_200k;
    p.cache_write_200k = cache_write_200k;
    Ok(Some(p))
}

fn apply_supplement(store: &mut Store, doc: &Value) {
    store.supplement.clear();
    store.fast_multipliers.clear();
    store.alias_rules.clear();
    store.dated_pricing.clear();
    // The feed is trusted by URL alone yet outranks both catalogs in
    // resolve(), so every value it hands us is range-checked on the way
    // in; drops are counted and reported once at the end.
    let mut dropped = 0usize;
    if let Some(pricing) = doc.get("pricing").and_then(Value::as_object) {
        for (model, entry) in pricing {
            match feed_card(entry) {
                Ok(Some(p)) => {
                    store.supplement.insert(model.clone(), p);
                }
                Ok(None) => continue,
                Err(()) => {
                    dropped += 1;
                    continue;
                }
            }
        }
    }
    correct_stale_supplement(&mut store.supplement);
    if let Some(mults) = doc.get("fast_multipliers").and_then(Value::as_object) {
        for (model, v) in mults {
            if let Some(m) = v.as_f64() {
                if m.is_finite() && (1.0..=MAX_FAST_MULTIPLIER).contains(&m) {
                    store.fast_multipliers.insert(model.clone(), m);
                } else {
                    dropped += 1;
                }
            }
        }
    }
    // 2026-07-31: OpenAI cut gpt-5.6-terra/-luna prices and both public
    // catalogs (and the supplement, which outranks them here) still carry
    // launch pricing. Correct ONLY the exact known-stale values — a
    // self-retiring override: the moment the supplement publishes any new
    // number for these models, its data wins again untouched. Remove this
    // whole function once upstream catches up.
    fn correct_stale_supplement(supplement: &mut HashMap<String, Price>) {
        let corrections: [(&str, f64, Price); 2] = [
            ("gpt-5.6-terra", 2.5, Price::flat(2.0, 12.0, 0.2, 2.5)),
            ("gpt-5.6-luna", 1.0, Price::flat(0.2, 1.2, 0.02, 0.25)),
        ];
        for (model, stale_input, corrected) in corrections {
            if let Some(p) = supplement.get_mut(model) {
                if (p.input - stale_input).abs() < 1e-9 {
                    *p = corrected;
                }
            }
        }
    }

    // The supplement is fetched from a third-party URL, so cap what it can
    // feed us: at most MAX_ALIAS_RULES of at most 256 chars each, compiled
    // with a bounded size, with plain-slug targets. (Rust's regex engine is
    // linear-time by design, so ReDoS-style backtracking blowups aren't
    // possible; the caps bound memory and compile cost.)
    if let Some(rules) = doc.get("alias_rules").and_then(Value::as_array) {
        for rule in rules.iter().take(MAX_ALIAS_RULES) {
            let (Some(pattern), Some(canonical)) = (
                rule.get("pattern").and_then(Value::as_str),
                rule.get("canonical").and_then(Value::as_str),
            ) else {
                continue;
            };
            // Targets become model names inside resolve() — plain ASCII
            // slugs only, so a rule can't smuggle control characters,
            // spaces, or unicode lookalikes into lookups.
            if pattern.len() > 256
                || canonical.len() > 128
                || canonical.is_empty()
                || !canonical.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'/' | b'_' | b'-'))
            {
                dropped += 1;
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
    // `dated_pricing`: promo windows / price changes shipped by PR to the
    // feed itself. Same entry shape as `pricing` plus optional `from`,
    // `until` (RFC3339; inclusive/exclusive) and `exclude_prefixes`
    // (case-insensitive, matched on the full slug). `model` is a bare
    // name — matching compares the slug's last segment only, so gateway
    // routes are handled with `exclude_prefixes`, not prefixed models.
    // Anything malformed — bad dates, from >= until, missing input/output,
    // implausible rates, a model that isn't a plain ASCII slug or
    // contains `/` — drops that entry.
    if let Some(entries) = doc.get("dated_pricing").and_then(Value::as_array) {
        let parse_ms = |v: Option<&Value>| -> Option<Option<i64>> {
            match v {
                None | Some(Value::Null) => Some(None),
                Some(Value::String(s)) => chrono::DateTime::parse_from_rfc3339(s)
                    .ok()
                    .map(|d| Some(d.timestamp_millis())),
                _ => None,
            }
        };
        for entry in entries.iter().take(256) {
            let Some(model) = entry.get("model").and_then(Value::as_str) else {
                dropped += 1;
                continue;
            };
            // A bare name only — matching compares the slug's last
            // segment, so a `/` in `model` could never match.
            if model.is_empty()
                || model.len() > 128
                || model.contains('/')
                || !model
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            {
                dropped += 1;
                continue;
            }
            let Ok(Some(p)) = feed_card(entry) else {
                dropped += 1;
                continue;
            };
            let (Some(from), Some(until)) = (
                parse_ms(entry.get("from")),
                parse_ms(entry.get("until")),
            )
            else {
                dropped += 1;
                continue;
            };
            if let (Some(f), Some(u)) = (from, until) {
                if f >= u {
                    dropped += 1;
                    continue;
                }
            }
            let exclude_prefixes = entry
                .get("exclude_prefixes")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(|p| p.to_lowercase())
                        .collect()
                })
                .unwrap_or_default();
            store.dated_pricing.push(DatedEntry {
                model: model.to_lowercase(),
                from,
                until,
                price: p,
                exclude_prefixes,
            });
        }
    }
    if dropped > 0 {
        eprintln!("[pane] pricing: supplement dropped {dropped} out-of-range entries");
    }
}

fn ingest(store: &mut Store, source: &str, doc: &Value) {
    match source {
        "litellm" => store.litellm = parse_litellm(doc),
        "modelsdev" => store.modelsdev = parse_modelsdev(doc),
        "aihubmix" => store.aihubmix = parse_aihubmix(doc),
        "openrouter" => {
            let (e, seg, dashseg) = parse_openrouter(doc);
            store.openrouter = e;
            store.openrouter_seg = seg;
            store.openrouter_dashseg = dashseg;
        }
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

    // Boot grace: a successful catalog download changes catalog_stamp(),
    // which discards the whole persisted spend cache and re-parses every
    // session log (gigabytes on long-lived installs). With autostart, the
    // daily refresh was landing exactly at Windows login — the one moment
    // the disk is already saturated. Serve yesterday's copy of any catalog
    // already on disk and refresh once the boot storm has passed; prices
    // are at most a day + grace stale. Gated per source: a catalog with no
    // file yet (first run, or a feed this machine can never reach) still
    // fetches immediately — no prices at all is worse, and one dead feed
    // must not disable the deferral for the others.
    static FIRST_CALL: OnceLock<std::time::Instant> = OnceLock::new();
    const BOOT_GRACE: std::time::Duration = std::time::Duration::from_secs(10 * 60);
    let in_grace =
        FIRST_CALL.get_or_init(std::time::Instant::now).elapsed() < BOOT_GRACE;

    let mut state = load_state();
    let now = now_ms();
    let unpriced = UNPRICED_HINT.load(std::sync::atomic::Ordering::Relaxed);
    // AIHubMix's catalog only matters to installs that route through the
    // gateway — everyone else never contacts aihubmix.com.
    let aihubmix_wanted = AIHUBMIX_WANTED.load(std::sync::atomic::Ordering::Relaxed)
        || providers::aihubmix::configured();
    for (source, url) in SOURCES {
        if source == "aihubmix" && !aihubmix_wanted {
            continue;
        }
        let entry = state.get(source).cloned().unwrap_or_else(|| json!({}));
        let fetched_at = entry.get("fetchedAt").and_then(Value::as_i64).unwrap_or(0);
        let failed_at = entry.get("failedAt").and_then(Value::as_i64).unwrap_or(0);
        let due = now - fetched_at > refresh_ms(source, unpriced) && now - failed_at > RETRY_MS;
        if !due {
            continue;
        }
        if in_grace && dir().join(format!("{source}.json")).exists() {
            continue;
        }
        let etag = entry.get("etag").and_then(Value::as_str).unwrap_or("").to_string();

        let result = tauri::async_runtime::block_on(async {
            let mut req = providers::http().get(url);
            if !etag.is_empty() {
                req = req.header("If-None-Match", etag.clone());
            }
            let mut resp = req.send().await.map_err(|e| e.to_string())?;
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
            // Read with a hard byte cap instead of resp.text() — the
            // declared Content-Length check alone wouldn't bound a
            // chunked/lying response.
            if resp.content_length().is_some_and(|l| l as usize > MAX_CATALOG_BYTES) {
                return Err("catalog larger than the size cap".into());
            }
            let mut bytes: Vec<u8> = Vec::new();
            while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
                if bytes.len() + chunk.len() > MAX_CATALOG_BYTES {
                    return Err("catalog larger than the size cap".into());
                }
                bytes.extend_from_slice(&chunk);
            }
            let body = String::from_utf8(bytes).map_err(|e| e.to_string())?;
            Ok((status, body, new_etag))
        });

        match result {
            Ok((304, _, _)) => {
                state[source] = json!({ "etag": etag, "fetchedAt": now, "failedAt": 0 });
            }
            Ok((_, body, new_etag)) => match serde_json::from_str::<Value>(&body) {
                Ok(doc) => {
                    // OpenRouter serves no ETag and churns daily in
                    // fields pane never reads (AIHubMix's catalog churns
                    // the same way) — store the normalized projection
                    // and treat identical prices like a 304: no write,
                    // no ingest, no spend-cache stamp churn.
                    let compacted = match source {
                        "openrouter" => Some(compact_openrouter(&doc)),
                        "aihubmix" => Some(compact_aihubmix(&doc)),
                        _ => None,
                    };
                    let body = if let Some(compacted) = compacted {
                        let path = dir().join(format!("{source}.json"));
                        if std::fs::read_to_string(&path).ok().as_deref()
                            == Some(compacted.as_str())
                        {
                            state[source] =
                                json!({ "etag": new_etag, "fetchedAt": now, "failedAt": 0 });
                            continue;
                        }
                        compacted
                    } else {
                        body
                    };
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

/// The supplement's fast-tier multiplier for `base` (a slug minus
/// `-fast`), 2.0 when none is published. Multipliers are keyed by the
/// plain model name, so effort/mode tokens peel off composed bases
/// ("gpt-5.6-sol-max" → "gpt-5.6-sol") until a key answers.
fn fast_tier_mult(s: &Store, base: &str) -> f64 {
    let mut mkey = base;
    loop {
        if let Some(m) = s.fast_multipliers.get(mkey) {
            return *m;
        }
        match ["-xhigh", "-light", "-low", "-medium", "-high", "-max", "-ultra"]
            .iter()
            .find_map(|suf| mkey.strip_suffix(suf))
        {
            Some(next) => mkey = next,
            None => return fable52_supplement_mult(s, base).unwrap_or(2.0),
        }
    }
}

/// Every rate in `p` multiplied by `m` — service tiers (fast, priority)
/// bill at a multiple of the base model's whole rate card.
fn scaled_price(p: &Price, m: f64) -> Price {
    let scale = |v: Option<f64>| v.map(|x| x * m);
    Price {
        input: p.input * m,
        output: p.output * m,
        cache_read: p.cache_read * m,
        cache_write: p.cache_write * m,
        input_200k: scale(p.input_200k),
        output_200k: scale(p.output_200k),
        cache_read_200k: scale(p.cache_read_200k),
        cache_write_200k: scale(p.cache_write_200k),
        cache_write_1h: scale(p.cache_write_1h),
        cache_write_1h_200k: scale(p.cache_write_1h_200k),
    }
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

    // A catalog row with zero input AND output rates is ambiguous: brand-new
    // slugs often land as 0/0 placeholders (qwen3.8-max), but free-tier and
    // local models are legitimately 0/0. Resolution order settles it — the
    // filtered chain below only takes entries with real rates, and 0/0
    // entries are reconsidered near the end (after the baked table), so a
    // placeholder loses to real rates anywhere while a model that is 0/0
    // in every source still prices as genuinely free, never as unpriced.
    let real = |p: &&Price| p.input > 0.0 || p.output > 0.0;
    if let Some(p) = s.supplement.get(&canonical).filter(real) {
        return Some(*p);
    }
    // Moonshot bills Pane's Kimi Code / API logs on its own card. LiteLLM
    // and models.dev mix reseller markups and stubs that drop cache-hit
    // rates, so first-party Kimi slugs take the published Moonshot table
    // before those catalogs. Other baked models (DeepSeek, Grok, Qwen)
    // still sit after catalogs so they self-retire.
    if let Some(p) = kimi_vendor_price(&canonical) {
        return Some(p);
    }
    // Kimi HighSpeed is a published 2× serving tier, not a distinct model.
    // Only peel known Kimi bases so an unrelated `-highspeed` slug cannot
    // inherit a 2× multiplier.
    if let Some(base) = canonical.strip_suffix("-highspeed") {
        if kimi_vendor_price(base).is_some() {
            if let Some(p) = resolve(s, base, depth + 1) {
                return Some(scaled_price(&p, 2.0));
            }
        }
    }
    if let Some(p) = s.litellm.get(&canonical).filter(real) {
        return Some(*p);
    }
    // Fast tier: base price × the supplement's multiplier (default 2). The
    // base runs through the whole chain — not a bare map lookup — so
    // composed slugs like "gpt-5.6-sol-max-fast" reach the -max fallback
    // and alias/fuzzy matching too.
    if let Some(base) = canonical.strip_suffix("-fast") {
        // Cognition's `-fast` is a Devin mode (`swe-1-6-fast`), not
        // Cursor's 2× SKU. Price at the SWE/Penguin card. Lightning is
        // its own 5× slug and never ends in `-fast`.
        let mut stem = base.rsplit('/').next().unwrap_or(base);
        for suf in ["-xhigh", "-light", "-low", "-medium", "-high", "-max", "-ultra"] {
            if let Some(next) = stem.strip_suffix(suf) {
                stem = next;
            }
        }
        if matches!(stem, "swe-1.7" | "swe-1-7" | "swe-1.6" | "swe-1-6" | "penguin")
            || stem == "swe-2"
            || stem.starts_with("swe-2-")
        {
            return resolve(s, base, depth + 1);
        }
        let m = fast_tier_mult(s, base);
        if let Some(p) = resolve(s, base, depth + 1) {
            return Some(scaled_price(&p, m));
        }
    }
    // Priority processing tier: some CLIs (Devin) bake the service tier
    // into the slug itself ("gpt-5.6-luna-xhigh-priority") instead of
    // flagging it per turn the way Codex rollouts do. OpenAI bills
    // priority at a per-model multiplier over the standard rate — keep
    // this table in sync with spend.rs's codex_priority_multiplier.
    if let Some(base) = canonical.strip_suffix("-priority") {
        let mut mkey = base;
        while let Some(next) = ["-xhigh", "-light", "-low", "-medium", "-high", "-max", "-ultra"]
            .iter()
            .find_map(|suf| mkey.strip_suffix(suf))
        {
            mkey = next;
        }
        let m = if matches!(mkey, "gpt-5.5" | "gpt-5.5-pro") { 2.5 } else { 2.0 };
        if let Some(p) = resolve(s, base, depth + 1) {
            return Some(scaled_price(&p, m));
        }
    }
    // LiteLLM fuzzy: provider-prefixed keys like "anthropic/claude-…".
    // Prefer an exact segment match; never fuzzy-match models.dev.
    if let Some(p) = s
        .litellm
        .iter()
        .find(|(k, p)| k.rsplit('/').next() == Some(canonical.as_str()) && real(p))
        .map(|(_, p)| *p)
    {
        return Some(p);
    }
    // models.dev keys third-party listings under DeepSeek's own provider
    // prefix (`deepseek/deepseek-v4.1-flash` is a reseller card, not the
    // platform's — the official rows live under bare ids in the `deepseek`
    // provider), so `deepseek/` V4.1-flash spellings take the baked
    // official card before that catalog. Scoped to that one family: it's
    // the only `deepseek/` slug whose builtin arm is the official card —
    // the v4-pro/v4-flash arms are AihubMix's gateway rates, and a direct
    // DeepSeek route must not bill those ahead of the catalogs. litellm's
    // `deepseek/` entries — the real platform card when upstream carries
    // them — still win above.
    if canonical.starts_with("deepseek/") && v41_flash_slug(&canonical) {
        if let Some(p) = builtin_price(&canonical) {
            return Some(p);
        }
    }
    if let Some(p) = s.modelsdev.get(&canonical).filter(real) {
        return Some(*p);
    }
    // Fable/Mythos 5.2 variants (suffixed, dotted, prefixed, dated) defer
    // to a catalog row for the plain base — an upstream `claude-fable-5-2`
    // entry outranks the baked card for every spelling, while an exact
    // variant row still wins above. Only the base itself falls to
    // builtin_price.
    if let Some(base) = fable52_base(&canonical) {
        if base != canonical.as_str() {
            if let Some(p) = resolve(s, base, depth + 1) {
                return Some(p);
            }
        }
    }
    // Vendor-documented rates for models the live catalogs haven't learned
    // yet — consulted after every online source so a real catalog entry
    // always wins the moment one ships. Keep this list tiny and sourced.
    if let Some(p) = builtin_price(&canonical) {
        return Some(p);
    }
    // OpenRouter lists brand-new models the same day they launch, but its
    // cards are flat — no long-context or 1h-cache tiers — so it sits
    // BELOW the hand-verified baked table: baked cards only retire when
    // LiteLLM or models.dev learn the model. Match exact ids, then the
    // last path segment, then a dot-normalized segment — each index
    // already holds the lexicographically smallest listing per tail.
    //
    // A row AIHubMix billed (`aihubmix/<id>`) that nothing above knows
    // also has the gateway's own published card. It is the last
    // real-rate source — below the baked AIHubMix arms (which carry what
    // the flat catalog can't) and below OpenRouter — so a price that
    // already resolved never moves. One exception: a vendor-qualified id
    // (`aihubmix/beta/novel`) that the gateway lists exactly outranks
    // OpenRouter's vendor-agnostic segment matches, which could
    // otherwise hand it another vendor's `novel`.
    let or_key = canonical.to_lowercase();
    let or_seg = or_key.rsplit('/').next().unwrap_or(or_key.as_str());
    let aihubmix_id = or_key.strip_prefix("aihubmix/");
    let aihubmix_card = |id: &str| {
        s.aihubmix
            .get(id)
            .or_else(|| s.aihubmix.get(id.rsplit('/').next().unwrap_or(id)))
    };
    if let Some(p) = s
        .openrouter
        .get(&or_key)
        .or_else(|| {
            aihubmix_id
                .filter(|id| id.contains('/'))
                .and_then(|id| s.aihubmix.get(id))
                .filter(real)
        })
        .or_else(|| s.openrouter_seg.get(or_seg))
        .or_else(|| s.openrouter_dashseg.get(or_seg.replace('.', "-").as_str()))
        .or_else(|| aihubmix_id.and_then(aihubmix_card).filter(real))
    {
        return Some(*p);
    }
    // DeepSeek ships dated snapshots ("deepseek-v4-pro-0813"): when no
    // source knows the dated form, retry the base slug through the WHOLE
    // chain — a catalog that has learned the base model must outrank the
    // baked table for dated spellings too, or the table would never
    // self-retire for them. Scoped to deepseek slugs with an all-digit
    // ≥4-char tail so version-bearing names never lose a real tail.
    // Last path segment so gateway prefixes (Fireworks, AihubMix, …)
    // still date-trim: Hermes logs
    // `accounts/fireworks/models/deepseek-v4-pro-0813`.
    let slug = canonical.rsplit('/').next().unwrap_or(canonical.as_str());
    if let Some((head, tail)) = slug.rsplit_once('-') {
        if slug.starts_with("deepseek")
            && tail.len() >= 4
            && tail.chars().all(|c| c.is_ascii_digit())
        {
            if let Some(p) = resolve(s, head, depth + 1) {
                return Some(p);
            }
        }
    }
    // Zero-rate entries, reconsidered: nothing anywhere carries real rates
    // for this slug, so a 0/0 catalog row means the model is genuinely
    // free — take it, keeping free models at $0.00 without an unpriced ⚠.
    if let Some(p) = s.supplement.get(&canonical) {
        return Some(*p);
    }
    if let Some(p) = s.litellm.get(&canonical) {
        return Some(*p);
    }
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
    if let Some(p) = aihubmix_id.and_then(aihubmix_card) {
        return Some(*p);
    }
    // Slug tails no catalog carries under their own name, billed at the
    // base model's per-token rates: reasoning-effort tiers (they change how
    // many tokens burn, not the unit price) and Cursor's Max/Ultra modes
    // (token-based at model rates). Only when the whole chain above misses
    // does one trailing token get peeled and the rest rerun — compositions
    // unwind right to left ("…-max-xhigh" → "…-max" → base), the depth cap
    // bounds it, and a real entry for any tail in any source always wins.
    for suffix in ["-xhigh", "-light", "-low", "-medium", "-high", "-max", "-ultra"] {
        if let Some(base) = canonical.strip_suffix(suffix) {
            return resolve(s, base, depth + 1);
        }
    }
    None
}

/// First-party Moonshot / Kimi rates from platform.kimi.ai (USD/MTok).
/// Last-segment match so CLI plan logs (`kimi-code/k3`), API prefixes
/// (`moonshot-ai/kimi-k3`), and Codex OAuth (`kimi-oauth/k3`) share one
/// table. Cache writes unpublished → input rate.
///
/// Generic tails (`k3`, `k2.5`) only match when the slug is bare or under
/// a Kimi/Moonshot prefix — `openrouter/k3` must not steal this card
/// ahead of that provider's catalog row. Tails that already say `kimi-`
/// / `moonshot-` (Fireworks `kimi-k2p7-code`) match under any prefix.
fn kimi_vendor_price(canonical: &str) -> Option<Price> {
    let bare = kimi_vendor_tail(canonical)?.to_ascii_lowercase();
    match bare.as_str() {

        // platform.kimi.ai/docs/pricing/chat — K3.1 Pro $2 in / $10 out /
        // $0.20 cache hit, Highspeed $4 / $20 / $0.20; cache writes take
        // the published 5-min (default TTL) rate — the input rate, same
        // convention as the K3 arm. Kimi Code plan logs spell these
        // `k3.1-pro` / `kimi-code/k3.1-pro`.
        "k3.1-pro" | "kimi-k3.1-pro" => Some(Price::flat(2.0, 10.0, 0.2, 2.0)),
        "k3.1-pro-highspeed" | "kimi-k3.1-pro-highspeed" => {
            Some(Price::flat(4.0, 20.0, 0.2, 4.0))
        }
        // platform.kimi.ai/docs/pricing/chat-k3 — k3-256k is the same API
        // rate card (the 2× figure on the membership page is plan *quota*,
        // not dollars).
        "k3" | "k3-256k" | "kimi-k3" | "kimi-k3-code" => Some(Price::flat(3.0, 15.0, 0.3, 3.0)),
        // platform.kimi.ai/docs/pricing/chat-k27-code — Kimi Code's
        // `kimi-for-coding` id is this model. HighSpeed is published at 2×.
        // `kimi-k2p7*` is Fireworks' spelling of the same SKU.
        "kimi-for-coding" | "kimi-k2.7-code" | "kimi-k2.7" | "k2.7-code"
        | "kimi-k2p7-code" | "kimi-k2p7" => Some(Price::flat(0.95, 4.0, 0.19, 0.95)),
        "kimi-for-coding-highspeed" | "kimi-k2.7-code-highspeed" | "k2.7-code-highspeed"
        | "kimi-k2p7-code-highspeed" => Some(Price::flat(1.90, 8.0, 0.38, 1.90)),
        // platform.kimi.ai/docs/pricing/chat-k26
        "kimi-k2.6" | "kimi-k2p6" | "k2.6" => Some(Price::flat(0.95, 4.0, 0.16, 0.95)),
        // platform.kimi.ai/docs/pricing/chat-k25
        "kimi-k2.5" | "kimi-k2p5" | "k2.5" => Some(Price::flat(0.60, 3.0, 0.10, 0.60)),
        // platform.kimi.ai/docs/pricing/chat-v1 — no cache-hit column;
        // unpublished cache bills at the input rate. Vision SKUs share
        // the same dollar card as the matching context size.
        "moonshot-v1-8k" | "moonshot-v1-8k-vision-preview" | "moonshot-v1-8k-0430" => {
            Some(Price::flat(0.20, 2.0, 0.20, 0.20))
        }
        "moonshot-v1-32k" | "moonshot-v1-32k-vision-preview" | "moonshot-v1-32k-0430" => {
            Some(Price::flat(1.0, 3.0, 1.0, 1.0))
        }
        "moonshot-v1-128k" | "moonshot-v1-128k-vision-preview" | "moonshot-v1-128k-0430" => {
            Some(Price::flat(2.0, 5.0, 2.0, 2.0))
        }
        _ => None,
    }
}

fn kimi_vendor_tail(canonical: &str) -> Option<&str> {
    let lower = canonical.to_ascii_lowercase();
    let tail = lower.rsplit('/').next().unwrap_or(lower.as_str());
    let named = tail.starts_with("kimi") || tail.starts_with("moonshot");
    let known_prefix = [
        "moonshot-ai/",
        "moonshotai/",
        "kimi-code/",
        "kimi-oauth/",
        "moonshot/",
        "kimi/",
    ]
    .iter()
    .any(|p| lower.starts_with(p));
    if named || known_prefix || !canonical.contains('/') {
        canonical.rsplit('/').next()
    } else {
        None
    }
}

/// Claude Fable 5.2 / Mythos 5.2 (launched 2026-11-09): one shared card —
/// $8 in / $40 out / $0.20 cache read / $10 5-min write / $16 1h write,
/// full 1M context at standard price (no >200k tier). Fast mode is a
/// flat 2×. LiteLLM, models.dev and the supplement don't carry it yet;
/// sitting in builtin_price means a real catalog entry wins the moment
/// one ships.
/// platform.claude.com/docs/en/about-claude/pricing
fn fable52_card() -> Price {
    Price { cache_write_1h: Some(16.0), ..Price::flat(8.0, 40.0, 0.20, 10.0) }
}

/// A supplement fast multiplier for a Fable/Mythos 5.2 spelling's base,
/// under either its dash or dotted key.
fn fable52_supplement_mult(s: &Store, model: &str) -> Option<f64> {
    let base = fable52_base(model)?;
    s.fast_multipliers
        .get(base)
        .or_else(|| s.fast_multipliers.get(base.replace("5-2", "5.2").as_str()))
        .copied()
}

/// The canonical dash base for Claude Fable/Mythos 5.2 spellings across
/// gateway spellings (`anthropic/…`, `vertex_ai/…@default`, Bedrock
/// `us.anthropic.…-v1:0`, `[1m]`, dated `-20261109` snapshots, effort
/// tails, Cursor's `Fable 5.2 (Auto)` prose). `-fast` is deliberately
/// NOT stripped — resolve()'s `-fast` arm owns the 2× tier. `5`, `5.1`,
/// `5-1`, `5-20`, `5.25` etc. must not match.
fn fable52_base(model: &str) -> Option<&'static str> {
    let mut m = model.to_ascii_lowercase();
    let last = m.rsplit('/').next().unwrap_or(&m).to_string();
    m = last;
    // Cursor-style prose: "fable 5.2" / "claude mythos 5.2" (+ optional
    // " (Auto)") — spaces instead of dashes.
    let bare = m.split(" (").next().unwrap_or(&m).trim().to_string();
    match bare.as_str() {
        "fable 5.2" | "claude fable 5.2" => return Some("claude-fable-5-2"),
        "mythos 5.2" | "claude mythos 5.2" => return Some("claude-mythos-5-2"),
        _ => {}
    }
    // Bedrock prefixes: [region.]anthropic.claude-…
    for region in ["us.", "eu.", "apac.", "global.", "us-gov."] {
        if let Some(rest) = m.strip_prefix(region) {
            m = rest.to_string();
            break;
        }
    }
    if let Some(rest) = m.strip_prefix("anthropic.") {
        m = rest.to_string();
    }
    // Vertex `@default`, Bedrock `-vN:M`, context tag `[1m]`.
    if let Some(at) = m.find('@') {
        m.truncate(at);
    }
    if let Some(b) = m.find("[1m]") {
        m.truncate(b);
    }
    if let Some(idx) = m.rfind("-v") {
        let tail = &m[idx + 2..];
        if tail.chars().all(|c| c.is_ascii_digit() || c == ':') && tail.contains(':') {
            m.truncate(idx);
        }
    }
    // Effort / reasoning tails repeat on some gateways.
    loop {
        match [
            "-xhigh", "-light", "-low", "-medium", "-high", "-max", "-ultra", "-thinking",
        ]
        .iter()
        .find_map(|suf| m.strip_suffix(suf))
        {
            Some(next) => m = next.to_string(),
            None => break,
        }
    }
    // One trailing 8-digit date stamp.
    if m.len() > 9 && m.as_bytes()[m.len() - 9] == b'-' {
        let d = &m[m.len() - 8..];
        if d.chars().all(|c| c.is_ascii_digit()) {
            m.truncate(m.len() - 9);
        }
    }
    match m.as_str() {
        "claude-fable-5-2" | "claude-fable-5.2" => Some("claude-fable-5-2"),
        "claude-mythos-5-2" | "claude-mythos-5.2" => Some("claude-mythos-5-2"),
        _ => None,
    }
}

fn builtin_price(canonical: &str) -> Option<Price> {
    // Gateways prefix the vendor slug (`xai/grok-4.6`, `deepseek/…`,
    // `accounts/fireworks/models/deepseek-v4-pro`). Peel to the last
    // path segment so one arm covers every spelling; catalogs still
    // outrank this table (except Kimi — see kimi_vendor_price) because
    // resolve() consults them first.
    if let Some(p) = kimi_vendor_price(canonical) {
        return Some(p);
    }
    if fable52_base(canonical).is_some_and(|b| b == canonical) {
        return Some(fable52_card());
    }
    match canonical {
        "aihubmix/hy4-preview" => return Some(Price::flat(0.845, 2.535, 0.04225, 0.845)),
        // AihubMix Tencent Hy4 (aihubmix.com/model/hy4): $0.8823 in /
        // $2.647 out / $0.4412 cache read; no published cache write →
        // billed at the input rate like the other AihubMix arms.
        "aihubmix/hy4" => return Some(Price::flat(0.8823, 2.647, 0.4412, 0.8823)),
        "aihubmix/qwen3.8-flash" => {
            return Some(Price::flat(0.1126, 0.380025, 0.014075, 0.175937));
        }
        // AihubMix Qwen3.8-Max-0902 (aihubmix.com/model/qwen3.8-max-2026-09-02):
        // $1.69 in / $5.07 out / $0.169 cache read / $2.1125 cache write.
        // Same headline card as their live `qwen3.8-max` page; this SKU is
        // the dated snapshot Hermes logs. Scoped so Alibaba's $2/$6
        // `qwen3.8-max` arm is untouched.
        "aihubmix/qwen3.8-max-2026-09-02" => {
            return Some(Price::flat(1.69, 5.07, 0.169, 2.1125));
        }
        // AihubMix MiniMax M3.1 Flash (aihubmix.com/model/minimax-m3.1-flash):
        // $0.165 in / $0.66 out / $0.0165 cache read; no published cache
        // write → billed at the input rate like the other AihubMix arms.
        "aihubmix/minimax-m3.1-flash" => {
            return Some(Price::flat(0.165, 0.66, 0.0165, 0.165));
        }
        // AihubMix Coding Qwen4 Max Preview
        // (aihubmix.com/model/coding-qwen4-max-preview): $0.353 in /
        // $1.059 out / $0.0441 cache read / $0.4412 cache write.
        "aihubmix/coding-qwen4-max-preview" => {
            return Some(Price::flat(0.353, 1.059, 0.0441, 0.4412));
        }
        _ => {}
    }
    let bare = canonical.rsplit('/').next().unwrap_or(canonical);
    match bare {
        // AihubMix DeepSeek V4 family — the gateway's OWN rate cards
        // (aihubmix.com/model/deepseek-v4-pro-0813 and /deepseek-v4-flash
        // headline pricing, NOT the cheaper Baidu/Tencent provider rows on
        // the same pages): pro $0.464/$0.928 with $0.004 cache read (yes,
        // ~1/116 of input — the page really says $0.004), flash
        // $0.154/$0.308 with $0.003 cache read. No cache-write rate is
        // published, so writes bill at the input rate. Hermes logs these
        // bare ("deepseek-v4-flash", "deepseek-v4-pro-0813" — verified
        // against a real state.db); public catalogs don't carry them yet.
        // Dated snapshots reach these arms via resolve()'s date-trim
        // retry, so a catalog that learns the base slug outranks them.
        "deepseek-v4-pro" => Some(Price::flat(0.464, 0.928, 0.004, 0.464)),
        "deepseek-v4-flash" => Some(Price::flat(0.154, 0.308, 0.003, 0.154)),
        // DeepSeek V4.1 Flash — official card effective 2026-09-10
        // (deepseek.com pricing): off-peak $0.15 in / $0.60 out /
        // $0.003 cache read. AihubMix routes bill the gateway's own card
        // (~3.3% over official, per aihubmix.com/model/deepseek-v4.1-flash).
        // Cache write is unpublished, so writes bill at input. Weekday
        // peak hours (01:00–04:00 and 06:00–10:00 UTC) bill at 2× the
        // whole card — applied per event by peak_multiplier(); events
        // before the changeover bill the flat launch card via
        // v41_flash_legacy_card(). Its own SKU — must not inherit
        // v4-flash's $0.154/$0.003 card. The hyphen spelling covers logs
        // that drop the version dot.
        "deepseek-v4.1-flash" | "deepseek-v4-1-flash" => {
            if canonical.starts_with("aihubmix/") {
                Some(Price::flat(0.155, 0.62, 0.0031, 0.155))
            } else {
                Some(Price::flat(0.15, 0.60, 0.003, 0.15))
            }
        }
        // Dated V4.1-flash snapshots ("deepseek-v4.1-flash-1017" — what
        // AIHubMix's Anthropic endpoint actually logs). Baked here, not
        // left to resolve()'s date-trim: that retry uses the bare base,
        // so an `aihubmix/` spelling would drop the gateway prefix and
        // bill whichever reseller card the catalog iterates first.
        // All-digit ≥4-char tail = a dated SKU, same card split.
        b if (b.starts_with("deepseek-v4.1-flash-") || b.starts_with("deepseek-v4-1-flash-"))
            && b.rsplit('-').next().is_some_and(|t| {
                t.len() >= 4 && t.chars().all(|c| c.is_ascii_digit())
            }) =>
        {
            if canonical.starts_with("aihubmix/") {
                Some(Price::flat(0.155, 0.62, 0.0031, 0.155))
            } else {
                Some(Price::flat(0.15, 0.60, 0.003, 0.15))
            }
        }
        // AihubMix GLM-5.3 preview (aihubmix.com/model/coding-glm-5.3):
        // $0.060 in / $0.220 out per MTok. No cache rate is published, so
        // reads/writes bill at the input rate. Only this gateway SKU is
        // baked — the generic vendor name `glm-5.3` is left unpriced so
        // Z.ai / OpenRouter / other scanners don't inherit the discount.
        // Hermes still prices its AihubMix `glm-5.3` rows by looking up
        // this SKU (see providers::hermes::price_lookup_slug).
        "coding-glm-5.3" => Some(Price::flat(0.06, 0.22, 0.06, 0.06)),
        // Alibaba Model Studio, GA'd 2026-08-03 (USD/MTok): input $2,
        // output $6, implicit cache read $0.25, explicit cache write $2.50.
        // Public catalogs still carry 0/0 placeholders for these slugs.
        "qwen3.8-max" | "qwen3.8-max-preview" => Some(Price::flat(2.0, 6.0, 0.25, 2.5)),
        // Grok 4.6, released 2026-08-12 — docs.x.ai/docs/pricing (USD/MTok):
        // $2 in / $0.50 cached / $6 out; prompts ≥200k bill $4 / $1 / $12
        // for the WHOLE request (xAI's long-context rule matches
        // request_cost's tiering, and Grok spend passes the default 200k
        // threshold). xAI bills no separate cache-write rate — writes are
        // plain input. The announced 2x "-fast" variant needs no entry:
        // the -fast resolution path applies the default 2x multiplier to
        // this rate card. Public catalogs don't carry 4.6 yet.
        // The cursor- spellings are Cursor's CSV branding for the same
        // vendor-billed model ("cursor-grok-4.6-xhigh" on launch day) —
        // matched EXPLICITLY rather than via a generic cursor- prefix
        // strip, so other cursor-branded slugs (which Cursor may bill at
        // its own rates) can never silently price off this baked table.
        "grok-4.6" | "grok-4-6" | "cursor-grok-4.6" | "cursor-grok-4-6" => Some(Price {
            input_200k: Some(4.0),
            output_200k: Some(12.0),
            cache_read_200k: Some(1.0),
            cache_write_200k: Some(4.0),
            ..Price::flat(2.0, 6.0, 0.5, 2.0)
        }),
        // GPT-6 Astra — OpenAI list (developers.openai.com/api/docs/models/gpt-6-astra):
        // $10 / $50 / $1 cache read / $12.50 cache write. Prompts above
        // 272k (Codex path) or 200k (generic request_cost) bill the
        // whole request at $20 / $75 / $2 / $25. Public catalogs do not
        // carry this slug yet; without a baked row, spend tiles go blank.
        "gpt-6-astra" => Some(Price {
            input_200k: Some(20.0),
            output_200k: Some(75.0),
            cache_read_200k: Some(2.0),
            cache_write_200k: Some(25.0),
            ..Price::flat(10.0, 50.0, 1.0, 12.5)
        }),
        // Gemini 3.8 Flash — Google API list through 2026-12-31
        // (ai.google.dev/gemini-api/docs/pricing): $0.75 in / $3.75 out /
        // $0.075 cache read / $0.75 cache write. Cursor effort tails
        // (-high, -xhigh) peel in resolve(); preview is its own SKU.
        "gemini-3.8-flash" | "gemini-3-8-flash" | "gemini-3.8-flash-preview"
        | "cursor-gemini-3.8-flash" => Some(Price::flat(0.75, 3.75, 0.075, 0.75)),
        // Gemini 4 Argon — AIHubMix billing config (aihubmix.com,
        // model_ratio 2 / completion_ratio 5 / cache_ratio 0.05, single
        // tier): $4.00 in / $20.00 out / $0.20 cache read, matching
        // Google's standard post-intro list ($4/$20, cached input 95%
        // off). Google's direct intro promo is $2/$10/$0.10 through
        // 2026-11-30 PT — dated_card()/gemini4_intro_card() overlay it for
        // bare routes, while AIHubMix traffic is scoped to this spelling
        // (aihubmix_scoped) so the gateway always bills $4/$20. Cache
        // write is unpublished, so writes bill at input.
        "gemini-4-argon" => Some(Price::flat(4.0, 20.0, 0.20, 4.0)),
        // Gemini 4 Carbon — Google's standard list card ($4 / $20 / $0.20
        // cache read from 2026-12-01; cloud.google.com/model-garden/
        // pricing). A base card so lookup() answers before any catalog
        // learns Carbon; dated_card()/gemini4_intro_card() overlay the
        // $2/$10/$0.10 intro rate before the cutoff. Cache write is
        // unpublished, so writes bill at input.
        "gemini-4-carbon" => Some(Price::flat(4.0, 20.0, 0.20, 4.0)),
        // Cognition SWE / Penguin — LiteLLM Cognition cost map
        // (docs.litellm.ai/docs/providers/cognition) and Devin's
        // in-app rate card: $0.50 / $2.50 / $0.20 cache read. Cache
        // write is unpublished, so writes bill at input. Devin CLI
        // logs hyphenated slugs (`swe-1-7`, `penguin-max`); dotted
        // LiteLLM spellings are included too. Lightning is the 5×
        // Cerebras tier.
        "swe-1.7" | "swe-1-7" | "swe-1.6" | "swe-1-6" | "penguin" => {
            Some(Price::flat(0.50, 2.50, 0.20, 0.50))
        }
        "swe-1.7-lightning" | "swe-1-7-lightning" => {
            Some(Price::flat(2.50, 12.50, 1.00, 2.50))
        }
        // Cognition SWE-2 (Windsurf model picker rate card, 2026-11):
        // SWE-2 is free and SWE-2 Flash is free for now — a 0/0 card
        // keeps them "free", never unpriced. SWE-2 Max Lightning bills
        // $1 in / $5 out / $0.10 cache read at the promo rate ("83% off
        // until 2027-01-01"); cache write unpublished → bills at input.
        "swe-2" | "swe-2-flash" => Some(Price::flat(0.0, 0.0, 0.0, 0.0)),
        "swe-2-max-lightning" => Some(Price::flat(1.0, 5.0, 0.10, 1.0)),
        // StepFun Step Plan (platform.stepfun.ai pricing, USD/MTok):
        // step-3.7-flash $0.20 in / $1.15 out / $0.04 cache hit;
        // step-3.5-flash (+ the -2603 dated SKU) $0.10 / $0.30 / $0.02.
        // No separate cache-write rate — writes bill at input. The
        // last-segment peel above covers `stepfun/step-*` gateway slugs.
        "step-3.7-flash" => Some(Price::flat(0.20, 1.15, 0.04, 0.20)),
        "step-3.5-flash" | "step-3.5-flash-2603" => Some(Price::flat(0.10, 0.30, 0.02, 0.10)),
        // step-5-preview is absent from StepFun's own price pages; the rate
        // comes from Artificial Analysis (artificialanalysis.ai/models/step-5,
        // 2026-09-19): $1.00 in / $2.70 out, 95% cache discount → $0.05.
        "step-5-preview" => Some(Price::flat(1.00, 2.70, 0.05, 1.00)),
        // StepFun audio models (platform.stepfun.ai pricing, USD/MTok).
        "stepaudio-2.5-realtime" => Some(Price::flat(1.50, 10.00, 0.30, 1.50)),
        "stepaudio-2.5-chat" => Some(Price::flat(1.50, 3.50, 0.30, 1.50)),
        // free (limited time) per StepFun's price list.
        "stepaudio-3-realtime-preview" | "stepaudio-3-chat-preview" => {
            Some(Price::flat(0.0, 0.0, 0.0, 0.0))
        }
        // CN-only models — ≈ from CNY list price, 7 元/$
        // (platform.stepfun.com): step-1o-turbo-vision 2.5/8/0.5 元,
        // step-1o-audio 25/60/5 元, step-audio-2 10/70/2 元,
        // step-audio-r1.5 10/105/2 元.
        "step-1o-turbo-vision" => Some(Price::flat(0.36, 1.14, 0.07, 0.36)),
        "step-1o-audio" => Some(Price::flat(3.57, 8.57, 0.71, 3.57)),
        "step-audio-2" => Some(Price::flat(1.43, 10.00, 0.29, 1.43)),
        "step-audio-r1.5" => Some(Price::flat(1.43, 15.00, 0.29, 1.43)),
        // Deliberately unpriced — StepFun publishes no token rate for
        // these, so rows keep the unpriced ⚠ instead of a guessed dollar
        // figure: step-router-v1, step-overture-preview, step-2x-large,
        // step-gui; step-image-edit-2 bills per image;
        // TTS/ASR bill per character/hour.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{request_cost, Price, Usage};

    fn usage(input: f64, output: f64, cache_read: f64, w5m: f64, w1h: f64) -> Usage {
        Usage { input, output, cache_read, cache_write_5m: w5m, cache_write_1h: w1h }
    }

    #[test]
    fn modelsdev_prefers_the_most_complete_reseller_entry() {
        // A stub reseller listing kimi-k3 without cache rates must not
        // shadow a complete entry (alphabetical order made "aaa" win before,
        // silently pricing $0.30 cache hits at the $3.00 input rate).
        let doc = serde_json::json!({
            "aaa-stub": { "models": { "kimi-k3": { "cost": { "input": 3.0, "output": 15.0 } } } },
            "moonshotai": { "models": { "kimi-k3": {
                "cost": { "input": 3.0, "output": 15.0, "cache_read": 0.3, "cache_write": 3.0 }
            } } },
        });
        let map = super::parse_modelsdev(&doc);
        let p = map.get("kimi-k3").expect("kimi-k3 parsed");
        assert_eq!((p.input, p.output, p.cache_read, p.cache_write), (3.0, 15.0, 0.3, 3.0));
    }

    #[test]
    fn zero_rate_catalog_placeholders_are_skipped() {
        let mut store = super::Store::default();
        store.litellm.insert("qwen-test".into(), super::Price::flat(0.0, 0.0, 0.0, 0.0));
        store.modelsdev.insert("qwen-test".into(), super::Price::flat(1.0, 5.0, 0.1, 1.25));
        // The 0/0 litellm placeholder must not shadow models.dev's real price.
        let p = super::resolve(&store, "qwen-test", 0).unwrap();
        assert!((p.input - 1.0).abs() < 1e-9);

        // Zero in EVERY source → the model is genuinely free: price $0.00
        // (no unpriced ⚠), like ":free" gateway variants and local models.
        store.modelsdev.insert("qwen-test".into(), super::Price::flat(0.0, 0.0, 0.0, 0.0));
        let free = super::resolve(&store, "qwen-test", 0).unwrap();
        assert_eq!((free.input, free.output), (0.0, 0.0));

        // The baked table outranks 0/0 placeholders: a catalog that lists
        // qwen3.8-max as 0/0 must not shadow Alibaba's documented rates.
        store.litellm.insert("qwen3.8-max".into(), super::Price::flat(0.0, 0.0, 0.0, 0.0));
        let baked = super::resolve(&store, "qwen3.8-max", 0).unwrap();
        assert!((baked.input - 2.0).abs() < 1e-9);
    }

    #[test]
    fn priority_slugs_price_at_base_times_priority_multiplier() {
        let mut store = super::Store::default();
        store
            .supplement
            .insert("gpt-5.6-luna".into(), super::Price::flat(0.2, 1.2, 0.02, 0.25));
        // Devin logs the service tier inside the slug; effort tokens between
        // the base and -priority must not break resolution.
        let p = super::resolve(&store, "gpt-5.6-luna-xhigh-priority", 0).unwrap();
        assert!((p.input - 0.4).abs() < 1e-9);
        assert!((p.output - 2.4).abs() < 1e-9);
        assert!((p.cache_read - 0.04).abs() < 1e-9);

        // gpt-5.5's priority tier is 2.5x, not the default 2x.
        store.supplement.insert("gpt-5.5".into(), super::Price::flat(10.0, 45.0, 1.0, 1.25));
        let p55 = super::resolve(&store, "gpt-5.5-priority", 0).unwrap();
        assert!((p55.input - 25.0).abs() < 1e-9);
    }

    #[test]
    fn stale_gpt56_supplement_prices_are_corrected_until_upstream_updates() {
        let stale = serde_json::json!({ "pricing": {
            "gpt-5.6-terra": { "input_per_million": 2.5, "output_per_million": 15.0,
                               "cache_read_per_million": 0.25, "cache_write_per_million": 3.125 },
            "gpt-5.6-luna":  { "input_per_million": 1.0, "output_per_million": 6.0,
                               "cache_read_per_million": 0.1, "cache_write_per_million": 1.25 },
        }});
        let mut store = super::Store::default();
        super::apply_supplement(&mut store, &stale);
        let terra = store.supplement.get("gpt-5.6-terra").unwrap();
        assert_eq!((terra.input, terra.output, terra.cache_read), (2.0, 12.0, 0.2));
        let luna = store.supplement.get("gpt-5.6-luna").unwrap();
        assert_eq!((luna.input, luna.output, luna.cache_read), (0.2, 1.2, 0.02));

        // Self-retiring: any NEW upstream number passes through untouched.
        let updated = serde_json::json!({ "pricing": {
            "gpt-5.6-terra": { "input_per_million": 1.75, "output_per_million": 11.0 },
        }});
        super::apply_supplement(&mut store, &updated);
        let terra = store.supplement.get("gpt-5.6-terra").unwrap();
        assert_eq!((terra.input, terra.output), (1.75, 11.0));
    }

    #[test]
    fn deepseek_v4_builtins_price_including_dated_snapshots() {
        let store = super::Store::default();
        // Real Hermes slugs: bare flash, dated pro snapshot, and the
        // deepseek/-prefixed catalog spelling.
        for slug in [
            "deepseek-v4-pro",
            "deepseek-v4-pro-0813",
            "deepseek/deepseek-v4-pro-0813",
            "accounts/fireworks/models/deepseek-v4-pro-0813",
        ] {
            let p = super::resolve(&store, slug, 0).unwrap_or_else(|| panic!("{slug} unpriced"));
            assert!((p.input - 0.464).abs() < 1e-9, "{slug}");
            assert!((p.output - 0.928).abs() < 1e-9, "{slug}");
            // AihubMix really does list $0.004/M cache read for pro —
            // ~1/116 of input is the vendor's number, not a typo.
            assert!((p.cache_read - 0.004).abs() < 1e-9, "{slug}");
        }
        for slug in ["deepseek-v4-flash", "deepseek-v4-flash-0731"] {
            let p = super::resolve(&store, slug, 0).unwrap_or_else(|| panic!("{slug} unpriced"));
            assert!((p.input - 0.154).abs() < 1e-9, "{slug}");
            assert!((p.cache_read - 0.003).abs() < 1e-9, "{slug}");
        }
        // V4.1 Flash's official card (effective 2026-09-10): off-peak
        // $0.15/$0.60/$0.003 — its own SKU, not v4-flash's card. Weekday
        // peak hours double it (peak_multiplier suite below). AihubMix
        // routes bill the gateway's ~3.3% markup card.
        for slug in ["deepseek-v4.1-flash", "deepseek-v4-1-flash", "deepseek/deepseek-v4.1-flash"] {
            let p = super::resolve(&store, slug, 0).unwrap_or_else(|| panic!("{slug} unpriced"));
            assert!((p.input - 0.15).abs() < 1e-9, "{slug}");
            assert!((p.output - 0.60).abs() < 1e-9, "{slug}");
            assert!((p.cache_read - 0.003).abs() < 1e-9, "{slug}");
        }
        let p = super::resolve(&store, "aihubmix/deepseek-v4.1-flash", 0).unwrap();
        assert!((p.input - 0.155).abs() < 1e-9 && (p.output - 0.62).abs() < 1e-9);
        assert!((p.cache_read - 0.0031).abs() < 1e-9);
        // The trimmer never eats non-date tails or other families.
        assert!(super::resolve(&store, "deepseek-v4", 0).is_none());
        assert!(super::resolve(&store, "deepseek-v3.2", 0).is_none());

        // Peak windows: 01:00–04:00 and 06:00–10:00 UTC on WEEKDAYS only,
        // and never before the 2026-09-10T04:00Z changeover. 2026-09-10 is
        // a Thursday; 2026-09-11 Friday; 2026-09-12 Saturday.
        let ms = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis();
        let peak = ["2026-09-10T06:00:00Z", "2026-09-10T08:00:00Z", "2026-09-11T01:00:00Z"];
        for t in peak {
            assert_eq!(super::peak_multiplier("deepseek-v4.1-flash", ms(t)), 2.0, "{t}");
            assert_eq!(super::peak_multiplier("aihubmix/deepseek-v4.1-flash", ms(t)), 2.0, "{t}");
            // Dated snapshot tails and effort suffixes peak too.
            assert_eq!(super::peak_multiplier("deepseek-v4.1-flash-0910", ms(t)), 2.0, "{t}");
            assert_eq!(super::peak_multiplier("deepseek-v4.1-flash-high", ms(t)), 2.0, "{t}");
        }
        let off = [
            "2026-09-11T00:59:00Z", // just before window 1
            "2026-09-11T04:00:00Z", // window 1 ends (exclusive)
            "2026-09-11T05:30:00Z", // between windows
            "2026-09-11T10:00:00Z", // window 2 ends
            "2026-09-11T23:00:00Z", // late night
            "2026-09-12T02:00:00Z", // Saturday in a peak window
            "2026-09-09T02:00:00Z", // in-window but before the changeover
        ];
        for t in off {
            assert_eq!(super::peak_multiplier("deepseek-v4.1-flash", ms(t)), 1.0, "{t}");
        }
        // Other models and the older flat SKU never scale.
        assert_eq!(super::peak_multiplier("gpt-5.6-sol", ms("2026-09-11T02:00:00Z")), 1.0);
        assert_eq!(super::peak_multiplier("deepseek-v4-flash", ms("2026-09-11T02:00:00Z")), 1.0);

        // Pre-changeover events bill the flat launch card; later ones don't.
        let legacy = super::v41_flash_legacy_card("deepseek-v4.1-flash", ms("2026-09-09T02:00:00Z"));
        assert!(legacy.is_some());
        let l = legacy.unwrap();
        assert!((l.input - 0.142).abs() < 1e-9 && (l.output - 0.284).abs() < 1e-9);
        assert!((l.cache_read - 0.0284).abs() < 1e-9);
        assert!(super::v41_flash_legacy_card("deepseek-v4.1-flash", ms("2026-09-11T02:00:00Z")).is_none());
        assert!(super::v41_flash_legacy_card("gpt-5.6-sol", ms("2026-09-09T02:00:00Z")).is_none());

        // Session-window overlap: fully inside, spanning a boundary,
        // weekend-only, pre-changeover clamp.
        assert_eq!(
            super::peak_overlap_ms(ms("2026-09-11T01:00:00Z"), ms("2026-09-11T02:00:00Z")),
            3_600_000
        );
        assert_eq!(
            super::peak_overlap_ms(ms("2026-09-11T00:30:00Z"), ms("2026-09-11T02:30:00Z")),
            5_400_000 // 01:00–02:30
        );
        assert_eq!(
            super::peak_overlap_ms(ms("2026-09-12T01:00:00Z"), ms("2026-09-12T02:00:00Z")),
            0
        );
        assert_eq!(
            super::peak_overlap_ms(ms("2026-09-09T02:00:00Z"), ms("2026-09-09T03:00:00Z")),
            0
        );

        // Self-retirement holds for dated spellings too: once any catalog
        // learns the BASE slug, dated snapshots follow the catalog, not
        // the baked table (Devin's find — the trim must run in resolve,
        // where the retry passes through every source, not in builtin).
        let mut store = super::Store::default();
        store.litellm.insert("deepseek-v4-pro".into(), super::Price::flat(0.5, 1.0, 0.05, 0.5));
        let p = super::resolve(&store, "deepseek-v4-pro-0813", 0).unwrap();
        assert!((p.input - 0.5).abs() < 1e-9);
        assert!((p.cache_read - 0.05).abs() < 1e-9);
        let p = super::resolve(&store, "accounts/fireworks/models/deepseek-v4-pro-0813", 0).unwrap();
        assert!((p.input - 0.5).abs() < 1e-9);
    }

    #[test]
    fn deepseek_prefixed_builtin_shortcut_only_covers_v41_flash() {
        let mut store = super::Store::default();
        // models.dev carries `deepseek/`-prefixed reseller listings. For
        // V4 pro the builtin arm is AihubMix's gateway card, so a direct
        // DeepSeek route must NOT take it — the catalog wins.
        store.modelsdev.insert(
            "deepseek/deepseek-v4-pro".into(),
            super::Price::flat(0.6, 1.2, 0.06, 0.6),
        );
        let p = super::resolve(&store, "deepseek/deepseek-v4-pro", 0).unwrap();
        assert!((p.input - 0.6).abs() < 1e-9);
        assert!((p.output - 1.2).abs() < 1e-9);
        assert!((p.cache_read - 0.06).abs() < 1e-9);

        // V4.1 Flash is the exception: its builtin arm IS the official
        // card, so a same-prefix reseller listing can't preempt it.
        store.modelsdev.insert(
            "deepseek/deepseek-v4.1-flash".into(),
            super::Price::flat(0.30, 1.20, 0.006, 0.30),
        );
        let p = super::resolve(&store, "deepseek/deepseek-v4.1-flash", 0).unwrap();
        assert!((p.input - 0.15).abs() < 1e-9);
        assert!((p.output - 0.60).abs() < 1e-9);
        assert!((p.cache_read - 0.003).abs() < 1e-9);
    }

    #[test]
    fn glm_53_aihubmix_preview_prices() {
        let store = super::Store::default();
        for slug in ["coding-glm-5.3", "aihubmix/coding-glm-5.3"] {
            let p = super::resolve(&store, slug, 0).unwrap_or_else(|| panic!("{slug} unpriced"));
            assert!((p.input - 0.06).abs() < 1e-9, "{slug}");
            assert!((p.output - 0.22).abs() < 1e-9, "{slug}");
            // No cache rate on the vendor page — unpublished → input rate.
            assert!((p.cache_read - 0.06).abs() < 1e-9, "{slug}");
        }
        // Generic vendor name (and gateway prefixes that peel to it) stay
        // unpriced until a catalog learns an official rate.
        assert!(super::resolve(&store, "glm-5.3", 0).is_none());
        assert!(super::resolve(&store, "z-ai/glm-5.3", 0).is_none());
        // A catalog that learns the official commercial slug outranks the
        // preview card (self-retirement).
        let mut store = super::Store::default();
        store.litellm.insert("glm-5.3".into(), super::Price::flat(1.0, 3.0, 0.25, 1.0));
        let p = super::resolve(&store, "glm-5.3", 0).unwrap();
        assert!((p.input - 1.0).abs() < 1e-9);
    }

    #[test]
    fn aihubmix_hy4_and_qwen38_flash_launch_prices_stay_scoped() {
        let store = super::Store::default();
        for slug in [
            "hy4",
            "tencent/hy4",
            "hy4-preview",
            "tencent/hy4-preview",
            "qwen3.8-flash",
            "qwen/qwen3.8-flash",
        ] {
            assert!(super::resolve(&store, slug, 0).is_none(), "{slug}");
        }

        let hy4 = super::resolve(&store, "aihubmix/hy4", 0).unwrap();
        assert_eq!(
            (hy4.input, hy4.output, hy4.cache_read, hy4.cache_write),
            (0.8823, 2.647, 0.4412, 0.8823)
        );
        let hy4 = super::resolve(&store, "aihubmix/hy4-preview", 0).unwrap();
        assert_eq!((hy4.input, hy4.output, hy4.cache_read), (0.845, 2.535, 0.04225));
        let qwen = super::resolve(&store, "aihubmix/qwen3.8-flash", 0).unwrap();
        assert_eq!(
            (qwen.input, qwen.output, qwen.cache_read, qwen.cache_write),
            (0.1126, 0.380025, 0.014075, 0.175937)
        );
    }

    #[test]
    fn aihubmix_qwen38_max_0902_uses_gateway_card() {
        let store = super::Store::default();
        let p = super::resolve(&store, "aihubmix/qwen3.8-max-2026-09-02", 0).unwrap();
        assert_eq!(
            (p.input, p.output, p.cache_read, p.cache_write),
            (1.69, 5.07, 0.169, 2.1125)
        );
        // Bare / other-gateway spellings stay off this discount card.
        assert!(super::resolve(&store, "qwen3.8-max-0902", 0).is_none());
        assert!(super::resolve(&store, "qwen3.8-max-2026-09-02", 0).is_none());
        // Alibaba's GA card is unchanged.
        let max = super::resolve(&store, "qwen3.8-max", 0).unwrap();
        assert_eq!((max.input, max.output), (2.0, 6.0));
        // A catalog row for the snapshot self-retires the bake.
        let mut store = super::Store::default();
        store.litellm.insert(
            "aihubmix/qwen3.8-max-2026-09-02".into(),
            super::Price::flat(9.0, 9.0, 0.9, 9.0),
        );
        let p = super::resolve(&store, "aihubmix/qwen3.8-max-2026-09-02", 0).unwrap();
        assert_eq!((p.input, p.output), (9.0, 9.0));
    }

    #[test]
    fn grok_46_builtin_prices_with_long_context_tier() {
        // Vendor rates (docs.x.ai/docs/pricing): $2/$0.50/$6, doubling for
        // ≥200k prompts — resolvable in every spelling before the public
        // catalogs learn the model. Empty store = builtin only.
        let store = super::Store::default();
        // cursor-grok-4.6-xhigh is the exact slug Cursor's CSV logged on
        // launch day — 20.6M real tokens showed $0.00 until it resolved.
        for slug in
            ["grok-4.6", "grok-4-6", "xai/grok-4.6", "grok-4.6-high", "cursor-grok-4.6-xhigh"]
        {
            let p = super::resolve(&store, slug, 0)
                .unwrap_or_else(|| panic!("{slug} did not price"));
            assert_eq!((p.input, p.output, p.cache_read, p.cache_write), (2.0, 6.0, 0.5, 2.0), "{slug}");
            assert_eq!(
                (p.input_200k, p.output_200k, p.cache_read_200k),
                (Some(4.0), Some(12.0), Some(1.0)),
                "{slug}"
            );
        }
        // The fast variant is "twice the price" (launch post): the -fast
        // path scales the whole rate card, long-context tier included.
        let fast = super::resolve(&store, "grok-4.6-fast", 0).unwrap();
        assert_eq!((fast.input, fast.output, fast.cache_read), (4.0, 12.0, 1.0));
        assert_eq!(fast.input_200k, Some(8.0));
    }

    #[test]
    fn gpt6_astra_and_gemini_38_flash_price_before_catalogs() {
        let store = super::Store::default();
        for slug in ["gpt-6-astra", "openai/gpt-6-astra", "gpt-6-astra-high"] {
            let p = super::resolve(&store, slug, 0)
                .unwrap_or_else(|| panic!("{slug} did not price"));
            assert_eq!(
                (p.input, p.output, p.cache_read, p.cache_write),
                (10.0, 50.0, 1.0, 12.5),
                "{slug}"
            );
            assert_eq!(
                (p.input_200k, p.output_200k, p.cache_read_200k),
                (Some(20.0), Some(75.0), Some(2.0)),
                "{slug}"
            );
        }
        let fast = super::resolve(&store, "gpt-6-astra-fast", 0).unwrap();
        assert_eq!((fast.input, fast.output, fast.cache_read), (20.0, 100.0, 2.0));

        for slug in [
            "gemini-3.8-flash",
            "google/gemini-3.8-flash",
            "gemini-3.8-flash-high",
            "gemini-3.8-flash-preview",
            "cursor-gemini-3.8-flash",
        ] {
            let p = super::resolve(&store, slug, 0)
                .unwrap_or_else(|| panic!("{slug} did not price"));
            assert_eq!(
                (p.input, p.output, p.cache_read, p.cache_write),
                (0.75, 3.75, 0.075, 0.75),
                "{slug}"
            );
        }

        for slug in [
            "gemini-4-argon",
            "aihubmix/gemini-4-argon",
            "google/gemini-4-argon",
            "gemini-4-argon-high",
        ] {
            let p = super::resolve(&store, slug, 0)
                .unwrap_or_else(|| panic!("{slug} did not price"));
            assert_eq!(
                (p.input, p.output, p.cache_read, p.cache_write),
                (4.0, 20.0, 0.20, 4.0),
                "{slug}"
            );
            assert!(p.input_200k.is_none(), "{slug}");
        }
    }

    #[test]
    fn gemini4_intro_card_is_dated_and_route_scoped() {
        let ms = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis();
        for slug in ["gemini-4-argon", "google/gemini-4-argon", "gemini-4-argon-high"] {
            let p = super::gemini4_intro_card(slug, ms("2026-10-14T12:00:00Z"))
                .unwrap_or_else(|| panic!("{slug} should intro-price"));
            assert_eq!(
                (p.input, p.output, p.cache_read, p.cache_write),
                (2.0, 10.0, 0.10, 2.0),
                "{slug}"
            );
        }
        // AIHubMix routes keep the gateway card even during the promo.
        assert!(super::gemini4_intro_card("aihubmix/gemini-4-argon", ms("2026-10-14T12:00:00Z")).is_none());
        // The promo ends 2026-11-30 PT = 2026-12-01T08:00Z.
        assert!(super::gemini4_intro_card("gemini-4-argon", ms("2026-12-01T07:59:59Z")).is_some());
        assert!(super::gemini4_intro_card("gemini-4-argon", ms("2026-12-01T08:00:00Z")).is_none());
        assert!(super::gemini4_intro_card("gemini-4-argon", ms("2026-12-05T00:00:00Z")).is_none());
        // Other models never take the intro card.
        assert!(super::gemini4_intro_card("gemini-4-flash", ms("2026-10-14T12:00:00Z")).is_none());
        assert!(super::gemini4_intro_card("gpt-6.1-sol", ms("2026-10-14T12:00:00Z")).is_none());
        // dated_card exposes the override to the spend paths on both
        // sides of the promo: intro inside the window, the enforced
        // list card after it.
        assert!(super::dated_card("gemini-4-argon", ms("2026-10-14T12:00:00Z")).is_some());
        let p = super::dated_card("gemini-4-argon", ms("2026-12-02T00:00:00Z"))
            .expect("post-intro argon takes the list card");
        assert_eq!((p.input, p.output, p.cache_read, p.cache_write), (4.0, 20.0, 0.20, 4.0));
    }

    #[test]
    fn gemini4_list_card_is_dated_and_route_scoped() {
        let ms = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis();
        // From the PT cutoff on the list card is enforced — the catalogs
        // still carry the intro rate then, so this must not defer to them.
        for slug in ["gemini-4-argon", "google/gemini-4-argon", "gemini-4-argon-high"] {
            let p = super::gemini4_list_card(slug, ms("2026-12-01T08:00:00Z"))
                .unwrap_or_else(|| panic!("{slug} should list-price"));
            assert_eq!(
                (p.input, p.output, p.cache_read, p.cache_write),
                (4.0, 20.0, 0.20, 4.0),
                "{slug}"
            );
        }
        // Inside the promo the intro card applies instead.
        assert!(super::gemini4_list_card("gemini-4-argon", ms("2026-12-01T07:59:59Z")).is_none());
        // AIHubMix keeps its flat gateway card year-round.
        assert!(super::gemini4_list_card("aihubmix/gemini-4-argon", ms("2026-12-05T00:00:00Z")).is_none());
        // Other models never take it.
        assert!(super::gemini4_list_card("gemini-4-flash", ms("2026-12-05T00:00:00Z")).is_none());
    }

    /// Devin bills Argon and Carbon at Google's price — no separate
    /// Devin card exists; turns price via dated_card's Google overlay.
    /// (devin_cost asserts the same numbers end-to-end in spend.rs.)
    #[test]
    fn gemini4_cards_cover_argon_and_carbon_by_date() {
        let ms = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis();
        for m in ["gemini-4-argon", "gemini-4-carbon"] {
            let p = super::gemini4_intro_card(m, ms("2026-10-14T12:00:00Z"))
                .unwrap_or_else(|| panic!("{m}"));
            assert_eq!((p.input, p.output, p.cache_read), (2.0, 10.0, 0.1), "{m}");
            // Promo ends 2026-11-30 PT = 2026-12-01T08:00Z.
            assert!(super::gemini4_intro_card(m, ms("2026-12-01T07:59:59Z")).is_some());
            assert!(super::gemini4_intro_card(m, ms("2026-12-01T08:00:00Z")).is_none());
            let p = super::gemini4_list_card(m, ms("2026-12-05T00:00:00Z"))
                .unwrap_or_else(|| panic!("{m}"));
            assert_eq!((p.input, p.output, p.cache_read), (4.0, 20.0, 0.2), "{m}");
        }
        assert!(super::gemini4_intro_card("gpt-6.1-sol", ms("2026-10-14T12:00:00Z")).is_none());
    }

    /// `hy4` prices on the AIHubMix card only when Claude Code logged it
    /// bare or explicitly AIHubMix-prefixed — `tencent/hy4` is Tencent's
    /// own endpoint and must not take the gateway rate.
    #[test]
    fn aihubmix_scoped_hy4_needs_a_bare_or_aihubmix_spelling() {
        assert_eq!(super::aihubmix_scoped("hy4").as_deref(), Some("aihubmix/hy4"));
        assert_eq!(super::aihubmix_scoped("HY4").as_deref(), Some("aihubmix/hy4"));
        assert_eq!(
            super::aihubmix_scoped("aihubmix/hy4").as_deref(),
            Some("aihubmix/hy4")
        );
        assert!(super::aihubmix_scoped("tencent/hy4").is_none());
        assert!(super::aihubmix_scoped("hy4-preview").is_none());
        // Verified-AIHubMix callers (pi/omp, DSH routes) still map any
        // prefixed hy4 spelling to the gateway SKU.
        assert_eq!(super::aihubmix_gateway_slug("tencent/hy4"), "aihubmix/hy4");
        assert_eq!(super::aihubmix_gateway_slug("HY4"), "aihubmix/hy4");
    }

    #[test]
    fn aihubmix_scoped_normalises_any_argon_spelling() {
        assert_eq!(
            super::aihubmix_scoped("gemini-4-argon").as_deref(),
            Some("aihubmix/gemini-4-argon")
        );
        assert_eq!(
            super::aihubmix_scoped("gemini-4-argon-high").as_deref(),
            Some("aihubmix/gemini-4-argon-high")
        );
        // Prefixed spellings on a verified AIHubMix route still normalise
        // to the gateway slug (segment case preserved).
        assert_eq!(
            super::aihubmix_scoped("google/gemini-4-argon").as_deref(),
            Some("aihubmix/gemini-4-argon")
        );
        assert_eq!(
            super::aihubmix_scoped("aihubmix/gemini-4-argon").as_deref(),
            Some("aihubmix/gemini-4-argon")
        );
        assert!(super::aihubmix_scoped("qwen3.8-max").is_none());
        assert!(super::aihubmix_scoped("google/qwen3.8-max").is_none());
    }

    #[test]
    fn aihubmix_coding_qwen4_max_preview_card_and_slug() {
        let store = super::Store::default();
        let p = super::resolve(&store, "aihubmix/coding-qwen4-max-preview", 0).unwrap();
        assert_eq!(
            (p.input, p.output, p.cache_read, p.cache_write),
            (0.353, 1.059, 0.0441, 0.4412)
        );
        for model in [
            "coding-qwen4-max-preview",
            "Coding-Qwen4-Max-Preview",
            "aihubmix/coding-qwen4-max-preview",
        ] {
            assert_eq!(
                super::aihubmix_scoped(model).as_deref(),
                Some("aihubmix/coding-qwen4-max-preview"),
                "{model}"
            );
            assert_eq!(super::aihubmix_gateway_slug(model), "aihubmix/coding-qwen4-max-preview");
        }
        // Alibaba's own preview slug stays off the gateway card.
        assert!(super::aihubmix_scoped("qwen4-max-preview").is_none());
    }

    #[test]
    fn aihubmix_minimax_m31_flash_card_and_slug() {
        let store = super::Store::default();
        let p = super::resolve(&store, "aihubmix/minimax-m3.1-flash", 0).unwrap();
        assert!((p.input - 0.165).abs() < 1e-9);
        assert!((p.output - 0.66).abs() < 1e-9);
        assert!((p.cache_read - 0.0165).abs() < 1e-9);
        // No published cache-write rate → billed at input like the other
        // AIHubMix arms.
        assert!((p.cache_write - 0.165).abs() < 1e-9);
        for model in [
            "minimax-m3.1-flash",
            "MiniMax-M3.1-Flash",
            "aihubmix/minimax-m3.1-flash",
        ] {
            assert_eq!(
                super::aihubmix_gateway_slug(model),
                "aihubmix/minimax-m3.1-flash",
                "{model}"
            );
        }
        assert_eq!(
            super::aihubmix_gateway_slug("deepseek-flash"),
            "aihubmix/deepseek-v4.1-flash"
        );
        assert_eq!(super::aihubmix_gateway_slug("glm-5.3-flash"), "glm-5.3-flash");
        // Claude's path stays untouched: the scoped helper never maps it.
        assert!(super::aihubmix_scoped("MiniMax-M3.1-Flash").is_none());
    }

    #[test]
    fn aihubmix_scoped_normalises_deepseek_flash_spellings() {
        // AIHubMix's own alias for deepseek-v4.1-flash-1017 — the catalog
        // card for the bare alias is flat with no peak logic, so it must
        // map onto the real gateway SKU.
        assert_eq!(
            super::aihubmix_scoped("deepseek-flash").as_deref(),
            Some("aihubmix/deepseek-v4.1-flash")
        );
        assert_eq!(
            super::aihubmix_scoped("DeepSeek-Flash").as_deref(),
            Some("aihubmix/deepseek-v4.1-flash")
        );
        // Dated snapshots keep their spelling — resolve() date-trims
        // them onto the same card and peak_multiplier sees the family.
        assert_eq!(
            super::aihubmix_scoped("deepseek-v4.1-flash-1017").as_deref(),
            Some("aihubmix/deepseek-v4.1-flash-1017")
        );
        // Other deepseek slugs are not AIHubMix routes.
        assert!(super::aihubmix_scoped("deepseek-v4-flash").is_none());
        assert!(super::aihubmix_scoped("deepseek-v4-pro").is_none());
        // Tencent Hy4 — exactly `hy4` on a verified AIHubMix route takes
        // the gateway card; the `-preview` SKU stays unrouted (hermes
        // maps it to `aihubmix/hy4-preview` itself).
        assert_eq!(
            super::aihubmix_scoped("hy4").as_deref(),
            Some("aihubmix/hy4")
        );
        assert_eq!(
            super::aihubmix_scoped("HY4").as_deref(),
            Some("aihubmix/hy4")
        );
        assert!(super::aihubmix_scoped("hy4-preview").is_none());
    }

    #[test]
    fn argon_fast_intro_keeps_its_tier_multiplier() {
        let ms = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis();
        let mut store = super::Store::default();
        // Default multiplier (2.0): the -fast slug's intro card scales.
        let p = super::gemini4_intro_card_in(&store, "gemini-4-argon-fast", ms("2026-10-14T12:00:00Z"))
            .expect("argon-fast should intro-price");
        assert_eq!((p.input, p.output, p.cache_read, p.cache_write), (4.0, 20.0, 0.20, 4.0));
        // A published 1.5 multiplier scales every rate.
        store.fast_multipliers.insert("gemini-4-argon".into(), 1.5);
        let p = super::gemini4_intro_card_in(&store, "gemini-4-argon-fast", ms("2026-10-14T12:00:00Z"))
            .expect("argon-fast should intro-price");
        for (got, want) in [
            (p.input, 3.0),
            (p.output, 15.0),
            (p.cache_read, 0.15),
            (p.cache_write, 3.0),
        ] {
            assert!((got - want).abs() < 1e-9, "{got} != {want}");
        }
        // Non-fast Argon is unscaled.
        let p = super::gemini4_intro_card_in(&store, "gemini-4-argon", ms("2026-10-14T12:00:00Z"))
            .expect("argon should intro-price");
        assert_eq!((p.input, p.output, p.cache_read, p.cache_write), (2.0, 10.0, 0.10, 2.0));
    }

    #[test]
    fn cognition_swe_and_penguin_price_before_catalogs() {
        let store = super::Store::default();
        // Devin hyphen slugs, LiteLLM dots, modes, gateway prefix.
        // `-fast` stays 1× (a Devin mode, not Cursor's 2× SKU).
        for slug in [
            "penguin",
            "penguin-max",
            "cognition/penguin",
            "swe-1-7",
            "swe-1.7",
            "swe-1-7-medium",
            "cognition/swe-1.7",
            "swe-1-6",
            "swe-1.6",
            "swe-1-6-fast",
        ] {
            let p = super::resolve(&store, slug, 0)
                .unwrap_or_else(|| panic!("{slug} did not price"));
            assert_eq!(
                (p.input, p.output, p.cache_read, p.cache_write),
                (0.50, 2.50, 0.20, 0.50),
                "{slug}"
            );
        }
        for slug in ["swe-1-7-lightning", "swe-1.7-lightning", "cognition/swe-1.7-lightning"] {
            let p = super::resolve(&store, slug, 0)
                .unwrap_or_else(|| panic!("{slug} did not price"));
            assert_eq!(
                (p.input, p.output, p.cache_read, p.cache_write),
                (2.50, 12.50, 1.00, 2.50),
                "{slug}"
            );
        }
    }

    #[test]
    fn swe2_free_and_lightning_promo_cards() {
        let store = super::Store::default();
        // Free models price at $0 — never an unpriced ⚠. Effort tails
        // and Devin's `-fast` mode stay on the free card.
        for slug in ["swe-2", "swe-2-flash", "swe-2-flash-high", "swe-2-fast"] {
            let p = super::resolve(&store, slug, 0)
                .unwrap_or_else(|| panic!("{slug} did not price"));
            assert_eq!(
                (p.input, p.output, p.cache_read, p.cache_write),
                (0.0, 0.0, 0.0, 0.0),
                "{slug}"
            );
        }
        // Max Lightning's promo card; `-fast` is Devin's mode, not a 2× SKU.
        for slug in [
            "swe-2-max-lightning",
            "swe-2-max-lightning-fast",
            "cognition/swe-2-max-lightning",
        ] {
            let p = super::resolve(&store, slug, 0)
                .unwrap_or_else(|| panic!("{slug} did not price"));
            assert_eq!(
                (p.input, p.output, p.cache_read, p.cache_write),
                (1.0, 5.0, 0.10, 1.0),
                "{slug}"
            );
        }
    }

    #[test]
    fn kimi_k31_pro_and_highspeed_price_every_spelling() {
        // Vendor-documented rates (platform.kimi.ai): Pro $2/$10/$0.20,
        // Highspeed $4/$20/$0.20 — resolvable however each tool spells it.
        for slug in [
            "kimi-k3.1-pro",
            "k3.1-pro",
            "kimi-code/k3.1-pro",
            "moonshot-ai/kimi-k3.1-pro",
            "moonshotai/kimi-k3.1-pro",
            "kimi-oauth/k3.1-pro",
            "k3.1-pro-max",
            "kimi-k3.1-pro-high",
        ] {
            let p = super::lookup(slug).unwrap_or_else(|| panic!("{slug} did not price"));
            assert_eq!(
                (p.input, p.output, p.cache_read, p.cache_write),
                (2.0, 10.0, 0.2, 2.0),
                "{slug}"
            );
        }
        for slug in [
            "kimi-k3.1-pro-highspeed",
            "k3.1-pro-highspeed",
            "kimi-code/k3.1-pro-highspeed",
            "moonshot-ai/kimi-k3.1-pro-highspeed",
        ] {
            let p = super::lookup(slug).unwrap_or_else(|| panic!("{slug} did not price"));
            assert_eq!(
                (p.input, p.output, p.cache_read, p.cache_write),
                (4.0, 20.0, 0.2, 4.0),
                "{slug}"
            );
        }
        // K3 unchanged.
        for slug in ["k3", "kimi-k3"] {
            let p = super::lookup(slug).unwrap_or_else(|| panic!("{slug} did not price"));
            assert_eq!((p.input, p.output, p.cache_read), (3.0, 15.0, 0.3), "{slug}");
        }
        // A foreign gateway's k3.1-pro must not steal Moonshot's card.
        assert!(super::kimi_vendor_price("openrouter/k3.1-pro").is_none());
    }

    #[test]
    fn kimi_k3_builtin_prices_every_spelling() {
        // Vendor-documented rates (platform.kimi.ai): $3 in, $15 out, $0.30
        // cache hit — resolvable however each tool spells the slug.
        for slug in [
            "kimi-k3",                    // Cursor / Devin bare
            "kimi-k3-code",               // Kimi Code CLI variant
            "k3",                         // Kimi Code plan id (`kimi-code/k3`)
            "k3-256k",                    // 256k K3 SKU, same API rates
            "moonshot/kimi-k3",           // catalog-style prefix
            "moonshot-ai/kimi-k3-code",   // Kimi CLI's own prefix
            "kimi-code/k3",               // plan login, last-segment peel
            "kimi-oauth/k3",              // Codex OAuth log
            "kimi-k3-high",               // effort tier → peels to base
            "kimi-k3-max",                // mode → peels to base
        ] {
            let p = super::lookup(slug).unwrap_or_else(|| panic!("{slug} did not price"));
            assert_eq!((p.input, p.output, p.cache_read), (3.0, 15.0, 0.3), "{slug}");
        }
    }

    #[test]
    fn kimi_k27_code_and_highspeed_plan_spellings() {
        // platform.kimi.ai/docs/pricing/chat-k27-code
        for slug in [
            "kimi-k2.7-code",
            "kimi-for-coding",
            "kimi-code/kimi-for-coding",
        ] {
            let p = super::lookup(slug).unwrap_or_else(|| panic!("{slug} did not price"));
            assert_eq!((p.input, p.output, p.cache_read), (0.95, 4.0, 0.19), "{slug}");
        }
        for slug in [
            "kimi-k2.7-code-highspeed",
            "kimi-for-coding-highspeed",
            "kimi-code/kimi-for-coding-highspeed",
        ] {
            let p = super::lookup(slug).unwrap_or_else(|| panic!("{slug} did not price"));
            assert_eq!((p.input, p.output, p.cache_read), (1.90, 8.0, 0.38), "{slug}");
        }
    }

    #[test]
    fn stepfun_builtins_price() {
        // Step Plan list prices (platform.stepfun.ai). Asserted at the
        // builtin layer — a live catalog row outranks these at lookup().
        let p = super::builtin_price("step-3.7-flash")
            .expect("step-3.7-flash did not price");
        assert_eq!((p.input, p.output, p.cache_read, p.cache_write), (0.20, 1.15, 0.04, 0.20));
        // The gateway spelling peels to the same card.
        let p = super::builtin_price("stepfun/step-3.7-flash")
            .expect("stepfun/step-3.7-flash did not reach the builtin");
        assert_eq!((p.input, p.output), (0.20, 1.15));
        for slug in ["step-3.5-flash", "step-3.5-flash-2603"] {
            let p = super::builtin_price(slug).unwrap_or_else(|| panic!("{slug} did not price"));
            assert_eq!(
                (p.input, p.output, p.cache_read, p.cache_write),
                (0.10, 0.30, 0.02, 0.10),
                "{slug}"
            );
        }
        // Token-billed audio + CN-only models (≈ USD from the CNY list).
        for (slug, want) in [
            ("stepaudio-2.5-realtime", (1.50, 10.00, 0.30, 1.50)),
            ("stepaudio-2.5-chat", (1.50, 3.50, 0.30, 1.50)),
            ("step-1o-turbo-vision", (0.36, 1.14, 0.07, 0.36)),
            ("step-1o-audio", (3.57, 8.57, 0.71, 3.57)),
            ("step-audio-2", (1.43, 10.00, 0.29, 1.43)),
            ("step-audio-r1.5", (1.43, 15.00, 0.29, 1.43)),
        ] {
            let p = super::builtin_price(slug).unwrap_or_else(|| panic!("{slug} did not price"));
            assert_eq!(
                (p.input, p.output, p.cache_read, p.cache_write),
                want,
                "{slug}"
            );
        }
        // Limited-time-free: prices to $0.00, not unpriced.
        for slug in ["stepaudio-3-realtime-preview", "stepaudio-3-chat-preview"] {
            let p = super::builtin_price(slug).unwrap_or_else(|| panic!("{slug} did not price"));
            assert_eq!((p.input, p.output), (0.0, 0.0), "{slug}");
        }
        // step-5-preview: Artificial Analysis rate ($1.00 / $2.70, 95% cache discount).
        let p = super::builtin_price("step-5-preview").expect("step-5-preview prices");
        assert_eq!((p.input, p.output, p.cache_read, p.cache_write), (1.00, 2.70, 0.05, 1.00));
        for slug in ["step-router-v1", "step-overture-preview"] {
            assert!(super::builtin_price(slug).is_none(), "{slug}");
        }
    }

    #[test]
    fn kimi_k26_k25_and_v1_use_published_rates() {
        for slug in ["kimi-k2.6", "moonshot-ai/kimi-k2.6", "kimi-k2p6"] {
            let p = super::lookup(slug).unwrap_or_else(|| panic!("{slug} did not price"));
            assert_eq!((p.input, p.output, p.cache_read), (0.95, 4.0, 0.16), "{slug}");
        }
        for slug in ["kimi-k2.5", "moonshot-ai/kimi-k2.5", "kimi-code/k2.5"] {
            let p = super::lookup(slug).unwrap_or_else(|| panic!("{slug} did not price"));
            assert_eq!((p.input, p.output, p.cache_read), (0.60, 3.0, 0.10), "{slug}");
        }
        for (slug, want) in [
            ("moonshot-v1-8k", (0.20, 2.0, 0.20)),
            ("moonshot-v1-8k-vision-preview", (0.20, 2.0, 0.20)),
            ("moonshot-v1-32k", (1.0, 3.0, 1.0)),
            ("moonshot-v1-128k", (2.0, 5.0, 2.0)),
            ("moonshot-ai/moonshot-v1-128k-vision-preview", (2.0, 5.0, 2.0)),
        ] {
            let p = super::lookup(slug).unwrap_or_else(|| panic!("{slug} did not price"));
            assert_eq!((p.input, p.output, p.cache_read), want, "{slug}");
        }
    }

    #[test]
    fn kimi_vendor_beats_reseller_catalog_stubs() {
        let mut store = super::Store::default();
        store.litellm.insert("kimi-k2.5".into(), Price::flat(0.60, 3.0, 0.60, 0.60));
        store.modelsdev.insert("kimi-k2.5".into(), Price::flat(0.50, 2.80, 0.125, 0.625));
        let p = super::resolve(&store, "kimi-k2.5", 0).unwrap();
        assert_eq!((p.input, p.output, p.cache_read), (0.60, 3.0, 0.10));
    }

    #[test]
    fn kimi_vendor_ignores_foreign_prefix_on_short_tails() {
        // Generic tails under another vendor must not steal Moonshot rates.
        assert!(super::kimi_vendor_price("openrouter/k3").is_none());
        assert!(super::kimi_vendor_price("acme/k2.5").is_none());
        assert!(super::kimi_vendor_price("groq/k2.7-code").is_none());
        // Bare plan ids and Kimi/Moonshot prefixes still match.
        assert!(super::kimi_vendor_price("k3").is_some());
        assert!(super::kimi_vendor_price("kimi-code/k3").is_some());
        assert!(super::kimi_vendor_price("Kimi-OAuth/K3").is_some());
        // A tail that already says kimi- is unambiguous even under Fireworks.
        assert!(super::kimi_vendor_price("accounts/fireworks/models/kimi-k2p7-code").is_some());
    }

    #[test]
    fn long_context_reprices_the_whole_request() {
        let mut p = Price::flat(3.0, 15.0, 0.3, 3.75);
        p.input_200k = Some(6.0);
        p.output_200k = Some(22.5);
        p.cache_read_200k = Some(0.6);

        // Under the threshold: base rates.
        let small = usage(150_000.0, 10_000.0, 0.0, 0.0, 0.0);
        let expect = (150_000.0 * 3.0 + 10_000.0 * 15.0) / 1e6;
        assert!((request_cost(&p, &small, true) - expect).abs() < 1e-9);

        // Prompt over 200k: every component reprices, output included.
        let big = usage(250_000.0, 10_000.0, 0.0, 0.0, 0.0);
        let expect = (250_000.0 * 6.0 + 10_000.0 * 22.5) / 1e6;
        assert!((request_cost(&p, &big, true) - expect).abs() < 1e-9);

        // Aggregated sources opt out and stay on base rates.
        let expect = (250_000.0 * 3.0 + 10_000.0 * 15.0) / 1e6;
        assert!((request_cost(&p, &big, false) - expect).abs() < 1e-9);

        // Cache reads count toward the threshold even with tiny input.
        let cached = usage(1_000.0, 0.0, 240_000.0, 0.0, 0.0);
        let expect = (1_000.0 * 6.0 + 240_000.0 * 0.6) / 1e6;
        assert!((request_cost(&p, &cached, true) - expect).abs() < 1e-9);
    }

    #[test]
    fn one_hour_cache_writes_bill_twice_input() {
        let p = Price::flat(4.0, 20.0, 0.4, 5.0);
        let u = usage(0.0, 0.0, 0.0, 0.0, 1_000_000.0);
        assert!((request_cost(&p, &u, true) - 8.0).abs() < 1e-9);

        // An explicit catalog rate wins over the ×2 convention.
        let mut p = p;
        p.cache_write_1h = Some(9.0);
        assert!((request_cost(&p, &u, true) - 9.0).abs() < 1e-9);
    }

    #[test]
    fn supplement_keeps_more_than_64_alias_rules() {
        // The live feed is past 100 rules; a 64-cap dropped Daybreak and
        // every Cursor Router "Auto Balanced" prose name.
        let mut rules = Vec::new();
        for i in 0..70 {
            rules.push(serde_json::json!({
                "pattern": format!("^dummy-{i}$"),
                "canonical": "auto"
            }));
        }
        rules.push(serde_json::json!({
            "pattern": "^(?:gpt-)?daybreak-blue-latest$",
            "canonical": "gpt-5.6-sol"
        }));
        let mut store = super::Store::default();
        super::apply_supplement(&mut store, &serde_json::json!({ "alias_rules": rules }));
        assert_eq!(store.alias_rules.len(), 71);
        assert!(store.alias_rules.iter().any(|(_, c)| c == "gpt-5.6-sol"));
    }

    #[test]
    fn supplement_drops_out_of_range_prices_and_multipliers() {
        // The supplement outranks every catalog in resolve(), so a hostile
        // or corrupt feed must not name its own dollars: nothing real
        // exceeds ~$600/M, and fast tiers stay within 1x-10x.
        let doc = serde_json::json!({
            "pricing": {
                "absurd-model": { "input_per_million": 50_000.0, "output_per_million": 1.0 },
                "negative-model": { "input_per_million": -1.0, "output_per_million": 1.0 },
                "bad-cache-model": { "input_per_million": 1.0, "output_per_million": 2.0,
                                     "cache_read_per_million": 99_999.0 },
                "ceiling-model": { "input_per_million": 10_000.0, "output_per_million": 10_000.0 },
                "sane-model": { "input_per_million": 3.0, "output_per_million": 15.0,
                                "cache_read_per_million": 0.3, "cache_write_per_million": 3.75 },
                "free-model": { "input_per_million": 0.0, "output_per_million": 0.0 },
            },
            "fast_multipliers": {
                "absurd-model": 500.0,
                "negative-model": 0.5,   // <1: fast cheaper than standard is nonsense
                "sane-model": 2.0,
                "ceiling-lo": 1.0,
                "ceiling-hi": 10.0,
            },
        });
        let mut store = super::Store::default();
        super::apply_supplement(&mut store, &doc);
        for model in ["absurd-model", "negative-model", "bad-cache-model"] {
            assert!(!store.supplement.contains_key(model), "{model}");
        }
        assert_eq!(store.fast_multipliers.get("absurd-model"), None);
        assert_eq!(store.fast_multipliers.get("negative-model"), None);
        let p = store.supplement.get("sane-model").unwrap();
        assert_eq!((p.input, p.output, p.cache_read, p.cache_write), (3.0, 15.0, 0.3, 3.75));
        assert_eq!(store.fast_multipliers.get("sane-model"), Some(&2.0));
        // Bounds are inclusive: the 0/0 free placeholder and the (unlikely
        // but in-range) ceiling values survive, so edge data never drops.
        assert!(store.supplement.contains_key("free-model"));
        assert!(store.supplement.contains_key("ceiling-model"));
        assert_eq!(store.fast_multipliers.get("ceiling-lo"), Some(&1.0));
        assert_eq!(store.fast_multipliers.get("ceiling-hi"), Some(&10.0));
    }

    #[test]
    fn supplement_alias_targets_are_plain_ascii_slugs() {
        // Targets land in resolve() as model names — anything but a plain
        // slug (control chars, spaces, unicode lookalikes, empty) drops.
        let doc = serde_json::json!({ "alias_rules": [
            { "pattern": "^good$", "canonical": "gpt-5.6-sol" },
            { "pattern": "^prefixed$", "canonical": "openai/gpt_5.6.sol" },
            { "pattern": "^control$", "canonical": "gpt-5.6\n-fake" },
            { "pattern": "^spaces$", "canonical": "not a slug" },
            { "pattern": "^unicode$", "canonical": "gрt-5.6" },
            { "pattern": "^empty$", "canonical": "" },
            { "pattern": "^long$", "canonical": "x".repeat(200) },
        ]});
        let mut store = super::Store::default();
        super::apply_supplement(&mut store, &doc);
        let targets: Vec<&str> = store.alias_rules.iter().map(|(_, c)| c.as_str()).collect();
        assert_eq!(targets, ["gpt-5.6-sol", "openai/gpt_5.6.sol"]);
    }

    /// Live probe: fetches the three catalogs and resolves a few real slugs.
    /// Run via `cargo test --lib pricing -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_probe() {
        super::ensure_fresh();
        let mut matrix: Vec<String> = vec![
            "claude-opus-4-8".into(),
            "gpt-5.1-codex-max-xhigh".into(),
            "composer-2.5".into(),
            "claude-4.5-haiku-thinking".into(),
            "gpt-5".into(),
            "some-unknown-model-xyz".into(),
        ];
        // The full GPT-5.6 family surface Cursor/Devin can emit: every
        // effort tier, Max/Ultra modes, fast tier, and their compositions.
        for base in ["gpt-5.6-luna", "gpt-5.6-terra", "gpt-5.6-sol"] {
            matrix.push(base.to_string());
            for suffix in [
                "-light", "-low", "-medium", "-high", "-xhigh", "-max", "-ultra", "-fast",
                "-max-xhigh", "-ultra-high", "-max-fast", "-fast-high",
                "-light-fast", "-xhigh-fast", "-ultra-fast", "-max-fast-xhigh",
            ] {
                matrix.push(format!("{base}{suffix}"));
            }
        }
        for model in &matrix {
            // Cap each dump line: this runs with --nocapture straight to
            // the terminal over data a third-party feed can influence.
            let line = match super::lookup(model) {
                Some(p) => format!(
                    "{model}: in=${:.2} out=${:.2} cr=${:.3} cw=${:.2} (per 1M)",
                    p.input, p.output, p.cache_read, p.cache_write
                ),
                None => format!("{model}: UNPRICED"),
            };
            eprintln!("{line:.200}");
        }
    }

    #[test]
    fn fable52_builtin_card_covers_every_spelling() {
        let s = super::Store::default();
        for m in [
            "claude-fable-5-2",
            "claude-fable-5.2",
            "claude-fable-5-2-xhigh",
            "claude-fable-5-2-20261109",
            "anthropic/claude-fable-5-2",
            "us.anthropic.claude-fable-5-2-v1:0",
            "vertex_ai/claude-fable-5-2@default",
            "claude-fable-5.2-thinking-high",
            "claude-fable-5-2[1m]",
            "claude-mythos-5-2",
            "Fable 5.2 (Auto)",
        ] {
            let p = super::resolve(&s, m, 0).unwrap_or_else(|| panic!("{m} unpriced"));
            assert_eq!((p.input, p.output, p.cache_read, p.cache_write), (8.0, 40.0, 0.2, 10.0), "{m}");
            assert_eq!(p.cache_write_1h, Some(16.0), "{m}");
            assert_eq!(p.input_200k, None, "{m}");
        }
    }

    #[test]
    fn fable52_fast_bills_two_x() {
        let s = super::Store::default();
        let p = super::resolve(&s, "claude-fable-5-2-xhigh-fast", 0).unwrap();
        assert_eq!((p.input, p.output, p.cache_read, p.cache_write), (16.0, 80.0, 0.4, 20.0));
        assert_eq!(p.cache_write_1h, Some(32.0));
    }

    #[test]
    fn fable52_lookalikes_stay_unpriced() {
        let s = super::Store::default();
        for m in [
            "claude-fable-5-1",
            "claude-fable-5",
            "claude-fable-5-20",
            "claude-fable-5-21",
            "claude-fable-5.25",
            "claude-opus-5-5",
        ] {
            assert!(super::resolve(&s, m, 0).is_none(), "{m} must not price");
        }
    }

    #[test]
    fn fable52_catalog_entry_outranks_the_builtin() {
        let mut s = super::Store::default();
        s.litellm
            .insert("claude-fable-5-2".into(), super::Price::flat(1.0, 2.0, 0.1, 1.5));
        let p = super::resolve(&s, "claude-fable-5-2", 0).unwrap();
        assert_eq!((p.input, p.output, p.cache_read, p.cache_write), (1.0, 2.0, 0.1, 1.5));
    }

    #[test]
    fn fable52_fast_multiplier_is_2x_unless_the_supplement_speaks() {
        let mut s = super::Store::default();
        assert_eq!(super::fast_multiplier_in(&s, "claude-fable-5-2"), 2.0);
        assert_eq!(super::fast_multiplier_in(&s, "claude-opus-9"), 1.0);
        s.fast_multipliers.insert("claude-fable-5-2".into(), 3.0);
        assert_eq!(super::fast_multiplier_in(&s, "claude-fable-5-2"), 3.0);
        // A suffixed spelling reads the base's multiplier; the dotted
        // supplement key works too.
        assert_eq!(super::fast_multiplier_in(&s, "claude-fable-5-2-xhigh"), 3.0);
        let mut s2 = super::Store::default();
        s2.fast_multipliers.insert("claude-fable-5.2".into(), 2.5);
        assert_eq!(super::fast_multiplier_in(&s2, "claude-fable-5-2-high"), 2.5);
    }

    /// A catalog row for the plain base outranks the baked card for every
    /// variant spelling — the builtin self-retires for the whole family.
    #[test]
    fn fable52_variants_defer_to_a_catalog_base() {
        let mut s = super::Store::default();
        s.litellm
            .insert("claude-fable-5-2".into(), super::Price::flat(9.0, 45.0, 0.3, 11.0));
        for m in [
            "claude-fable-5-2-xhigh",
            "claude-fable-5.2",
            "anthropic/claude-fable-5-2",
            "claude-fable-5-2-20261109",
        ] {
            let p = super::resolve(&s, m, 0).unwrap_or_else(|| panic!("{m} unpriced"));
            assert_eq!((p.input, p.output), (9.0, 45.0), "{m}");
        }
        let fast = super::resolve(&s, "claude-fable-5-2-xhigh-fast", 0).unwrap();
        assert_eq!((fast.input, fast.output), (18.0, 90.0));
        // An exact-variant catalog row still outranks the base for that
        // variant.
        s.litellm
            .insert("claude-fable-5-2-xhigh".into(), super::Price::flat(7.0, 35.0, 0.2, 9.0));
        let p = super::resolve(&s, "claude-fable-5-2-xhigh", 0).unwrap();
        assert_eq!((p.input, p.output), (7.0, 35.0));
    }

    /// The `-fast` SKU arm honors the same supplement fallback as
    /// fast_multiplier_in: a dotted supplement key scales the whole card.
    #[test]
    fn fable52_fast_sku_reads_the_supplement_fallback() {
        let mut s = super::Store::default();
        s.fast_multipliers.insert("claude-fable-5.2".into(), 2.5);
        for m in ["claude-fable-5-2-fast", "claude-fable-5-2-xhigh-fast"] {
            let p = super::resolve(&s, m, 0).unwrap_or_else(|| panic!("{m} unpriced"));
            assert_eq!((p.input, p.output), (20.0, 100.0), "{m}");
        }
        assert_eq!(super::fast_multiplier_in(&s, "claude-fable-5-2"), 2.5);
        let empty = super::Store::default();
        let p = super::resolve(&empty, "claude-fable-5-2-fast", 0).unwrap();
        assert_eq!((p.input, p.output), (16.0, 80.0));
    }

    /// `-fast` on the Fable/Mythos 5.2 bases prices at 2× through the
    /// existing fast arm (Devin keeps the SKU after reordering).
    #[test]
    fn fable52_base_fast_spellings_price_two_x() {
        let s = super::Store::default();
        for m in ["claude-fable-5-2-fast", "claude-mythos-5-2-fast"] {
            let p = super::resolve(&s, m, 0).unwrap_or_else(|| panic!("{m} unpriced"));
            assert_eq!((p.input, p.output), (16.0, 80.0), "{m}");
        }
    }

    // ---------- OpenRouter source ----------

    #[test]
    fn parse_openrouter_rates_strings_and_numbers() {
        let doc = serde_json::json!({"data": [
            {"id": "google/gemini-4-carbon", "pricing": {
                "prompt": "0.000002", "completion": "0.00001",
                "input_cache_read": "0.0000001", "input_cache_write": "0.0000001875"}},
            {"id": "anthropic/claude-fable-5.2", "pricing": {
                "prompt": 0.000008, "completion": 0.00004,
                "input_cache_read": 0.0000002, "input_cache_write": 0.00001}},
            {"id": "openai/miss-cache", "pricing": {
                "prompt": "0.000001", "completion": "0.000002"}},
        ]});
        let (exact, seg, _) = super::parse_openrouter(&doc);
        let c = &exact["google/gemini-4-carbon"];
        assert_eq!((c.input, c.output, c.cache_write), (2.0, 10.0, 0.1875));
        assert!((c.cache_read - 0.1).abs() < 1e-9);
        let f = &exact["anthropic/claude-fable-5.2"];
        assert_eq!((f.input, f.output, f.cache_write), (8.0, 40.0, 10.0));
        assert!((f.cache_read - 0.2).abs() < 1e-9);
        // Missing cache fields fall back to input.
        let m = &exact["openai/miss-cache"];
        assert_eq!((m.cache_read, m.cache_write), (1.0, 1.0));
        assert_eq!(seg.len(), 3);
    }

    #[test]
    fn parse_openrouter_skips_variants_meta_negative_and_free() {
        let doc = serde_json::json!({"data": [
            {"id": "x/a:free", "pricing": {"prompt": "0", "completion": "0"}},
            {"id": "x/a:batch", "pricing": {"prompt": "0.000001", "completion": "0.000002"}},
            {"id": "openrouter/auto", "pricing": {"prompt": "-1", "completion": "-1"}},
            {"id": "x/negative", "pricing": {"prompt": "-0.000001", "completion": "0.000002"}},
            {"id": "x/zero", "pricing": {"prompt": "0", "completion": "0"}},
            {"id": "x/implausible", "pricing": {"prompt": "99999", "completion": "0.1"}},
        ]});
        let (exact, seg, dashseg) = super::parse_openrouter(&doc);
        assert!(exact.is_empty() && seg.is_empty() && dashseg.is_empty());
    }

    #[test]
    fn openrouter_resolves_segment_and_dot_spellings() {
        let mut s = super::Store::default();
        let doc = serde_json::json!({"data": [
            {"id": "acme/brand-new-model", "pricing": {
                "prompt": "0.000002", "completion": "0.00001"}},
            {"id": "acme/foo-1.5", "pricing": {
                "prompt": "0.000008", "completion": "0.00004"}},
            {"id": "zzz/brand-new-model", "pricing": {
                "prompt": "0.000007", "completion": "0.00003"}},
        ]});
        super::ingest(&mut s, "openrouter", &doc);
        // Segment match across vendor prefixes; smallest id (acme/ <
        // zzz/) wins the tie deterministically.
        let p = super::resolve(&s, "brand-new-model", 0).unwrap();
        assert_eq!((p.input, p.output), (2.0, 10.0));
        let p = super::resolve(&s, "other/brand-new-model", 0).unwrap();
        assert_eq!((p.input, p.output), (2.0, 10.0));
        // A model with a baked arm (Carbon) keeps the baked card.
        let mut s2 = super::Store::default();
        super::ingest(&mut s2, "openrouter", &serde_json::json!({"data": [
            {"id": "google/gemini-4-carbon", "pricing": {
                "prompt": "0.000002", "completion": "0.00001"}},
        ]}));
        assert_eq!(super::resolve(&s2, "gemini-4-carbon", 0).unwrap().input, 4.0);
        // Dot/dash: `foo-1-5` finds acme/foo-1.5 via the dash index.
        let p = super::resolve(&s, "foo-1-5", 0).unwrap();
        assert_eq!((p.input, p.output), (8.0, 40.0));
        // But a hand-verified baked card outranks OpenRouter — flat
        // catalog cards lack the 1h tier the baked fable card carries.
        let p = super::resolve(&s, "claude-fable-5-2", 0).unwrap();
        assert_eq!(p.cache_write_1h, Some(16.0));
    }

    #[test]
    fn openrouter_loses_to_litellm_and_modelsdev() {
        let mut s = super::Store::default();
        s.litellm.insert("claude-fable-5-2".into(), super::Price::flat(9.0, 45.0, 0.3, 11.0));
        s.modelsdev.insert("gemini-4-carbon".into(), super::Price::flat(5.0, 25.0, 0.5, 6.0));
        let doc = serde_json::json!({"data": [
            {"id": "acme/foo-1.5", "pricing": {
                "prompt": "0.000008", "completion": "0.00004"}},
            {"id": "google/gemini-4-carbon", "pricing": {
                "prompt": "0.000002", "completion": "0.00001"}},
        ]});
        super::ingest(&mut s, "openrouter", &doc);
        assert_eq!(super::resolve(&s, "claude-fable-5-2", 0).unwrap().input, 9.0);
        assert_eq!(super::resolve(&s, "gemini-4-carbon", 0).unwrap().input, 5.0);
    }

    // ---------- supplement optional fields ----------

    #[test]
    fn supplement_optional_tiers_fill_the_card() {
        let mut s = super::Store::default();
        let doc = serde_json::json!({"pricing": {
            "x/model": {"input_per_million": 1, "output_per_million": 2,
                "cache_read_per_million": 0.1, "cache_write_per_million": 1.5,
                "cache_write_1h_per_million": 3,
                "input_above_200k_per_million": 2,
                "output_above_200k_per_million": 4,
                "cache_read_above_200k_per_million": 0.2,
                "cache_write_above_200k_per_million": 3},
            "x/bad": {"input_per_million": 1, "output_per_million": 2,
                "input_above_200k_per_million": 99999},
        }});
        super::apply_supplement(&mut s, &doc);
        let p = &s.supplement["x/model"];
        assert_eq!(p.cache_write_1h, Some(3.0));
        assert_eq!(p.input_200k, Some(2.0));
        assert_eq!(p.output_200k, Some(4.0));
        assert_eq!(p.cache_read_200k, Some(0.2));
        assert_eq!(p.cache_write_200k, Some(3.0));
        assert!(!s.supplement.contains_key("x/bad"));
    }

    // ---------- dated_pricing feed entries ----------

    fn dated_doc() -> serde_json::Value {
        serde_json::json!({"dated_pricing": [
            {"model": "gemini-4-carbon", "from": "2026-12-01T08:00:00Z",
                "until": null, "input_per_million": 4, "output_per_million": 20,
                "cache_read_per_million": 0.2, "cache_write_per_million": 4,
                "exclude_prefixes": ["aihubmix/"]},
            {"model": "promo-model", "from": "2026-11-01T00:00:00Z",
                "until": "2026-12-01T00:00:00Z",
                "input_per_million": 1, "output_per_million": 5},
        ]})
    }

    #[test]
    fn dated_pricing_window_prefix_and_suffix_rules() {
        let ms = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis();
        let mut s = super::Store::default();
        super::apply_supplement(&mut s, &dated_doc());
        // Before/inside/after the window (until is exclusive).
        assert!(super::feed_dated_card(&s, "gemini-4-carbon", ms("2026-11-30T12:00:00Z")).is_none());
        let p = super::feed_dated_card(&s, "gemini-4-carbon", ms("2026-12-01T08:00:00Z")).unwrap();
        assert_eq!((p.input, p.output, p.cache_read), (4.0, 20.0, 0.2));
        assert_eq!(
            super::feed_dated_card(&s, "promo-model", ms("2026-11-15T00:00:00Z")).unwrap().input,
            1.0
        );
        assert!(super::feed_dated_card(&s, "promo-model", ms("2026-12-01T00:00:00Z")).is_none());
        // Open-ended entry stays live.
        assert!(super::feed_dated_card(&s, "gemini-4-carbon", ms("2030-01-01T00:00:00Z")).is_some());
        // exclude_prefixes keeps the gateway spelling off the card.
        assert!(super::feed_dated_card(&s, "aihubmix/gemini-4-carbon", ms("2027-01-01T00:00:00Z")).is_none());
        // Effort tails peel to the entry's model…
        assert!(super::feed_dated_card(&s, "promo-model-xhigh-high", ms("2026-11-15T00:00:00Z")).is_some());
        // …but a different stem must NOT match.
        assert!(super::feed_dated_card(&s, "gemini-4-carbon-cyber-permissive", ms("2027-01-01T00:00:00Z")).is_none());
        // -fast scales by fast_tier_mult (2x default).
        let p = super::feed_dated_card(&s, "promo-model-fast", ms("2026-11-15T00:00:00Z")).unwrap();
        assert_eq!((p.input, p.output), (2.0, 10.0));
    }

    #[test]
    fn dated_pricing_invalid_entries_drop() {
        let mut s = super::Store::default();
        let doc = serde_json::json!({"dated_pricing": [
            {"model": "bad date", "input_per_million": 1, "output_per_million": 2},
            {"model": "ok", "from": "2026-12-01T00:00:00Z", "until": "2026-11-01T00:00:00Z",
                "input_per_million": 1, "output_per_million": 2},
            {"model": "ok", "input_per_million": 1},
            {"model": "ok", "input_per_million": 99999, "output_per_million": 2},
            {"model": "vendor/has-slash", "input_per_million": 1, "output_per_million": 2},
            {"model": "fine", "input_per_million": 1, "output_per_million": 2},
        ]});
        super::apply_supplement(&mut s, &doc);
        assert_eq!(s.dated_pricing.len(), 1);
        assert_eq!(s.dated_pricing[0].model, "fine");
    }

    #[test]
    fn dated_feed_fingerprint_tracks_applicable_entries() {
        let mut s = super::Store::default();
        assert_eq!(super::dated_feed_fingerprint_in(&s, "gemini-4-carbon"), "");
        super::apply_supplement(&mut s, &dated_doc());
        let fp = super::dated_feed_fingerprint_in(&s, "gemini-4-carbon-high");
        assert!(!fp.is_empty());
        // Excluded route and unrelated models see no entry.
        assert_eq!(super::dated_feed_fingerprint_in(&s, "aihubmix/gemini-4-carbon"), "");
        assert_eq!(super::dated_feed_fingerprint_in(&s, "gemini-4-argon"), "");
        // A fast tier folds its multiplier in.
        assert!(super::dated_feed_fingerprint_in(&s, "promo-model-fast").contains("fast=2"));
        // Editing the window or the rate changes the fingerprint.
        let mut doc = dated_doc();
        doc["dated_pricing"][0]["input_per_million"] = serde_json::json!(5);
        super::apply_supplement(&mut s, &doc);
        assert_ne!(super::dated_feed_fingerprint_in(&s, "gemini-4-carbon"), fp);
        let mut doc = dated_doc();
        doc["dated_pricing"][0]["from"] = serde_json::json!("2026-12-02T08:00:00Z");
        super::apply_supplement(&mut s, &doc);
        assert_ne!(super::dated_feed_fingerprint_in(&s, "gemini-4-carbon"), fp);
    }

    #[test]
    fn dated_pricing_overrides_the_builtin_dated_card() {
        let ms = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis();
        let mut s = super::Store::default();
        let doc = serde_json::json!({"dated_pricing": [
            {"model": "gemini-4-argon", "input_per_million": 3, "output_per_million": 15},
        ]});
        super::apply_supplement(&mut s, &doc);
        // Feed entry (open window) outranks the baked Argon intro card.
        let p = super::feed_dated_card(&s, "gemini-4-argon", ms("2026-10-14T12:00:00Z")).unwrap();
        assert_eq!((p.input, p.output), (3.0, 15.0));
    }

    #[test]
    fn shipped_supplement_feed_parses_clean() {
        // The checked-in feed must ingest with zero dropped entries —
        // every pricing key, multiplier, alias rule and dated entry
        // survives validation.
        let doc: serde_json::Value =
            serde_json::from_str(include_str!("../../pricing/supplement.json")).unwrap();
        let mut s = super::Store::default();
        super::apply_supplement(&mut s, &doc);
        assert_eq!(s.supplement.len(), doc["pricing"].as_object().unwrap().len());
        assert_eq!(
            s.fast_multipliers.len(),
            doc["fast_multipliers"].as_object().unwrap().len()
        );
        assert_eq!(
            s.alias_rules.len(),
            doc["alias_rules"].as_array().unwrap().len()
        );
        assert!(s.dated_pricing.is_empty());
    }

    // ---------- Gemini 4 Carbon dated card ----------

    #[test]
    fn gemini4_carbon_intro_and_list_cards() {
        let ms = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis();
        // AIHubMix lists Carbon at Google's rate, so its spelling follows
        // the same dated card.
        for m in [
            "gemini-4-carbon",
            "google/gemini-4-carbon",
            "gemini-4-carbon-high",
            "aihubmix/gemini-4-carbon",
        ] {
            let p = super::gemini4_intro_card(m, ms("2026-10-14T12:00:00Z"))
                .unwrap_or_else(|| panic!("{m}"));
            assert_eq!((p.input, p.output, p.cache_read), (2.0, 10.0, 0.1), "{m}");
            let p = super::gemini4_list_card(m, ms("2027-01-01T00:00:00Z"))
                .unwrap_or_else(|| panic!("{m}"));
            assert_eq!((p.input, p.output, p.cache_read), (4.0, 20.0, 0.2), "{m}");
        }
        // AIHubMix Argon stays on the gateway's own flat card.
        assert!(super::gemini4_intro_card("aihubmix/gemini-4-argon", ms("2026-10-14T12:00:00Z")).is_none());
        assert!(super::gemini4_list_card("aihubmix/gemini-4-argon", ms("2027-01-01T00:00:00Z")).is_none());
        // lookup() answers for Carbon even before any catalog lists it —
        // the base card, which dated_card() overlays before the cutoff.
        let store = super::Store::default();
        for m in ["gemini-4-carbon", "google/gemini-4-carbon", "aihubmix/gemini-4-carbon"] {
            let p = super::resolve(&store, m, 0).unwrap_or_else(|| panic!("{m}"));
            assert_eq!((p.input, p.output, p.cache_read), (4.0, 20.0, 0.2), "{m}");
        }
        // cyber-permissive was never on the promo: no intro, list after.
        assert!(super::gemini4_intro_card("gemini-4-carbon-cyber-permissive", ms("2026-10-14T12:00:00Z")).is_none());
        assert!(super::gemini4_intro_card("gemini-4-argon-cyber-permissive", ms("2026-10-14T12:00:00Z")).is_none());
        assert!(super::gemini4_list_card("gemini-4-carbon-cyber-permissive", ms("2027-01-01T00:00:00Z")).is_some());
    }

    #[test]
    fn compact_openrouter_normalizes_and_stabilizes() {
        let doc = serde_json::json!({"data": [
            {"id": "zzz/b-1", "pricing": {"prompt": "0.000002", "completion": "0.00001"},
                "description": "churns daily", "context_length": 1000000},
            {"id": "a/a-1", "pricing": {"prompt": 0.000001, "completion": 0.000002},
                "description": "other", "per_request_limits": {}},
            {"id": "x/a:free", "pricing": {"prompt": "0", "completion": "0"}},
            {"id": "openrouter/auto", "pricing": {"prompt": "-1", "completion": "-1"}},
        ]});
        let compacted: serde_json::Value =
            serde_json::from_str(&super::compact_openrouter(&doc)).unwrap();
        // Same three maps the raw doc parses to — compaction loses
        // nothing parse_openrouter consumes.
        assert_eq!(
            super::parse_openrouter(&compacted),
            super::parse_openrouter(&doc)
        );
        let rows = compacted["data"].as_array().unwrap();
        // Sorted by id, only the four price fields, rejected ids gone.
        let ids: Vec<&str> = rows.iter().filter_map(|r| r["id"].as_str()).collect();
        assert_eq!(ids, ["a/a-1", "zzz/b-1"]);
        assert_eq!(
            rows[0]["pricing"].as_object().unwrap().keys().collect::<Vec<_>>(),
            vec!["completion", "input_cache_read", "input_cache_write", "prompt"]
        );
        assert!(rows[0].get("description").is_none());
        // Non-price churn compacts to identical bytes.
        let mut churned = doc.clone();
        churned["data"][0]["description"] = serde_json::json!("brand new text");
        churned["data"][0]["supported_parameters"] = serde_json::json!(["tools"]);
        assert_eq!(super::compact_openrouter(&doc), super::compact_openrouter(&churned));
        // A price move does change the bytes.
        let mut moved = doc.clone();
        moved["data"][0]["pricing"]["prompt"] = serde_json::json!("0.000003");
        assert_ne!(super::compact_openrouter(&doc), super::compact_openrouter(&moved));
    }

    // ---------- AIHubMix catalog ----------

    fn aihubmix_doc() -> serde_json::Value {
        serde_json::json!({"success": true, "data": [
            {"model_id": "new-llm-x", "types": "llm", "desc": "long text",
                "pricing": {"input": 0.353, "output": 1.059, "cache_read": 0.0441, "cache_write": 0.4412}},
            {"model_id": "Vendor/Mixed-Case-1T", "types": "llm,search",
                "pricing": {"input": 2, "output": 10}},
            {"model_id": "free-llm", "types": "llm", "pricing": {"input": 0, "output": 0}},
            {"model_id": "auto", "types": "llm", "pricing": {"input": 2, "output": 2}},
            {"model_id": "an-image-model", "types": "image_generation",
                "pricing": {"input": 5, "output": 30}},
            {"model_id": "no-output", "types": "llm", "pricing": {"input": 1}},
            {"model_id": "implausible", "types": "llm", "pricing": {"input": 99999, "output": 1}},
            {"model_id": "negative", "types": "llm", "pricing": {"input": -1, "output": 1}},
        ]})
    }

    #[test]
    fn parse_aihubmix_keeps_text_cards_only() {
        let map = super::parse_aihubmix(&aihubmix_doc());
        let p = &map["new-llm-x"];
        assert_eq!(
            (p.input, p.output, p.cache_read, p.cache_write),
            (0.353, 1.059, 0.0441, 0.4412)
        );
        // Unpublished cache rates bill at input; ids key lowercase, and
        // by last segment too.
        let m = &map["vendor/mixed-case-1t"];
        assert_eq!((m.input, m.output, m.cache_read, m.cache_write), (2.0, 10.0, 2.0, 2.0));
        assert_eq!(map["mixed-case-1t"].input, 2.0);
        assert!(map.contains_key("free-llm"));
        for skipped in ["auto", "an-image-model", "no-output", "implausible", "negative"] {
            assert!(!map.contains_key(skipped), "{skipped}");
        }
    }

    #[test]
    fn compact_aihubmix_round_trips_and_stabilizes() {
        let doc = aihubmix_doc();
        let compacted: serde_json::Value =
            serde_json::from_str(&super::compact_aihubmix(&doc)).unwrap();
        assert_eq!(super::parse_aihubmix(&compacted), super::parse_aihubmix(&doc));
        let rows = compacted["data"].as_array().unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r["model_id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["Vendor/Mixed-Case-1T", "free-llm", "new-llm-x"]);
        assert!(rows[0].get("desc").is_none() && rows[0].get("types").is_none());
        // Non-price churn compacts to identical bytes; a price move doesn't.
        let mut churned = doc.clone();
        churned["data"][0]["desc"] = serde_json::json!("rewritten");
        assert_eq!(super::compact_aihubmix(&doc), super::compact_aihubmix(&churned));
        let mut moved = doc.clone();
        moved["data"][0]["pricing"]["input"] = serde_json::json!(0.4);
        assert_ne!(super::compact_aihubmix(&doc), super::compact_aihubmix(&moved));
    }

    #[test]
    fn aihubmix_catalog_prices_only_gateway_slugs_as_a_last_resort() {
        let mut s = super::Store::default();
        super::ingest(&mut s, "aihubmix", &aihubmix_doc());
        // Only `aihubmix/…` slugs see the catalog — a bare name stays
        // unpriced, so another route never takes the gateway's rate.
        assert!(super::resolve(&s, "new-llm-x", 0).is_none());
        let p = super::resolve(&s, "aihubmix/new-llm-x", 0).unwrap();
        assert_eq!((p.input, p.output), (0.353, 1.059));
        assert_eq!(super::resolve(&s, "AIHubMix/New-LLM-X", 0).unwrap().input, 0.353);
        // Full id and last-segment spellings, and effort tails peel.
        assert_eq!(super::resolve(&s, "aihubmix/vendor/mixed-case-1t", 0).unwrap().input, 2.0);
        assert_eq!(super::resolve(&s, "aihubmix/mixed-case-1t", 0).unwrap().input, 2.0);
        assert_eq!(super::resolve(&s, "aihubmix/new-llm-x-high", 0).unwrap().input, 0.353);
        // A model free on the gateway prices at $0 rather than unpriced.
        let free = super::resolve(&s, "aihubmix/free-llm", 0).unwrap();
        assert_eq!((free.input, free.output), (0.0, 0.0));
        assert!(super::resolve(&s, "aihubmix/an-image-model", 0).is_none());
        // Baked AIHubMix arms and the public catalogs still win.
        let mut s = super::Store::default();
        super::ingest(&mut s, "aihubmix", &serde_json::json!({"data": [
            {"model_id": "hy4", "types": "llm", "pricing": {"input": 9, "output": 9}},
            {"model_id": "new-llm-x", "types": "llm", "pricing": {"input": 9, "output": 9}},
        ]}));
        assert_eq!(super::resolve(&s, "aihubmix/hy4", 0).unwrap().input, 0.8823);
        s.litellm.insert("aihubmix/new-llm-x".into(), super::Price::flat(1.0, 2.0, 0.1, 1.0));
        assert_eq!(super::resolve(&s, "aihubmix/new-llm-x", 0).unwrap().input, 1.0);
    }

    #[test]
    fn aihubmix_catalog_slug_spellings() {
        assert_eq!(super::aihubmix_catalog_slug("New-LLM-X"), "aihubmix/new-llm-x");
        // The vendor stays, so the catalog's exact id is tried first.
        assert_eq!(super::aihubmix_catalog_slug("vendor/New-LLM-X"), "aihubmix/vendor/new-llm-x");
        assert_eq!(super::aihubmix_catalog_slug("AIHubMix/vendor/x"), "aihubmix/vendor/x");
    }

    #[test]
    fn aihubmix_catalog_exact_id_beats_a_shared_segment_and_openrouter_wins() {
        // Two vendors share a model name: each id gets its own card, and
        // only an unlisted vendor falls back to the segment alias.
        let mut s = super::Store::default();
        super::ingest(&mut s, "aihubmix", &serde_json::json!({"data": [
            {"model_id": "alpha/novel", "types": "llm", "pricing": {"input": 1, "output": 1}},
            {"model_id": "beta/novel", "types": "llm", "pricing": {"input": 3, "output": 3}},
        ]}));
        let price = |m: &str| super::resolve(&s, &super::aihubmix_catalog_slug(m), 0).unwrap().input;
        assert_eq!(price("alpha/novel"), 1.0);
        assert_eq!(price("beta/novel"), 3.0);
        assert_eq!(price("gamma/novel"), 1.0);
        assert_eq!(price("novel"), 1.0);
        // A slug OpenRouter already priced keeps that price…
        super::ingest(&mut s, "openrouter", &serde_json::json!({"data": [
            {"id": "vendor/novel", "pricing": {"prompt": "0.000007", "completion": "0.00003"}},
        ]}));
        assert_eq!(super::resolve(&s, "aihubmix/novel", 0).unwrap().input, 7.0);
        assert_eq!(super::resolve(&s, "aihubmix/gamma/novel", 0).unwrap().input, 7.0);
        // …but OpenRouter's vendor-agnostic `novel` must not shadow a
        // vendor-qualified id the gateway lists exactly.
        assert_eq!(super::resolve(&s, "aihubmix/alpha/novel", 0).unwrap().input, 1.0);
        assert_eq!(super::resolve(&s, "aihubmix/beta/novel", 0).unwrap().input, 3.0);
        // An exact OpenRouter id still comes first.
        super::ingest(&mut s, "openrouter", &serde_json::json!({"data": [
            {"id": "aihubmix/beta/novel", "pricing": {"prompt": "0.000005", "completion": "0.00002"}},
        ]}));
        assert_eq!(super::resolve(&s, "aihubmix/beta/novel", 0).unwrap().input, 5.0);
    }

    #[test]
    fn shipped_alias_rule_gemini3_pro_matches_all_forms() {
        let doc: serde_json::Value =
            serde_json::from_str(include_str!("../../pricing/supplement.json")).unwrap();
        let rule = doc["alias_rules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["canonical"].as_str() == Some("gemini-3-pro-preview"))
            .unwrap();
        let re = regex::Regex::new(rule["pattern"].as_str().unwrap()).unwrap();
        for m in [
            "gemini-3-pro-preview",
            "gemini-3-pro-preview-high",
            "gemini-3-pro-preview-preview",
            "gemini-3-pro-preview-high-preview",
            "gemini-3-pro-high",
            "gemini-3-pro-low-preview",
        ] {
            assert!(re.is_match(m), "{m}");
        }
        for m in ["gemini-3-pro", "gemini-3-pro-ultra", "gemini-3-pro-preview-ultra"] {
            assert!(!re.is_match(m), "{m}");
        }
    }

    // ---------- per-source refresh ----------

    #[test]
    fn refresh_intervals_by_source() {
        assert_eq!(super::refresh_ms("supplement", false), 6 * 3600 * 1000);
        assert_eq!(super::refresh_ms("litellm", false), 24 * 3600 * 1000);
        assert_eq!(super::refresh_ms("openrouter", false), 24 * 3600 * 1000);
        assert_eq!(super::refresh_ms("supplement", true), 3_600_000);
        assert_eq!(super::refresh_ms("openrouter", true), 3_600_000);
    }

}
