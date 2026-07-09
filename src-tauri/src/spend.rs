//! Local spend computation — the "Total Spend" dashboard, per-provider
//! Today / Yesterday / Last 30 Days rows, per-model breakdowns, and the
//! 30-day Usage Trend series. Mirrors the macOS app: costs are derived
//! from the session logs each CLI already writes on this machine, so
//! nothing is sent anywhere.
//!
//! Large logs are handled with a per-file cache keyed by (mtime, size):
//! only files that changed since the last refresh are re-parsed.

use chrono::{DateTime, Datelike, Local, Utc};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use crate::pricing;
use crate::providers;

pub const TREND_DAYS: usize = 30;

#[derive(Serialize, Clone)]
pub struct ModelSpend {
    pub model: String,
    pub cost: f64,
    pub tokens: f64,
}

#[derive(Serialize, Clone, Default)]
pub struct Window {
    pub cost: f64,
    pub tokens: f64,
    pub models: Vec<ModelSpend>,
}

#[derive(Serialize, Clone)]
pub struct ProviderSpend {
    pub id: &'static str,
    pub name: &'static str,
    pub today: Window,
    pub yesterday: Window,
    pub last30: Window,
    /// Tokens per day, oldest first — trend[29] is today.
    pub trend: Vec<f64>,
    /// Events excluded because no catalog prices their model — counting
    /// them at a guessed rate would fabricate dollars (Mac #853 semantics).
    pub unpriced: u64,
    pub unpriced_models: Vec<String>,
}

impl ProviderSpend {
    fn has_data(&self) -> bool {
        self.last30.cost > 0.004 || self.last30.tokens > 0.0 || self.unpriced > 0
    }
}

/// (local calendar day, model) → (cost, tokens). Day = days since CE.
type DayMap = HashMap<(i32, String), (f64, f64)>;

/// Everything one file contributes: priced per-day totals plus the tally of
/// unpriced (excluded) events per model name. Cached as a unit so exclusion
/// counts survive the per-file cache.
#[derive(Default, Clone)]
struct FileData {
    days: DayMap,
    unpriced: HashMap<String, u64>,
}

struct FileEntry {
    mtime: SystemTime,
    size: u64,
    /// Pricing-catalog generation the file was priced under — a catalog
    /// refresh re-prices even files that haven't changed on disk.
    gen: u64,
    data: FileData,
}

fn cache() -> &'static Mutex<HashMap<PathBuf, FileEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, FileEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn day_of_utc(ts: DateTime<Utc>) -> i32 {
    ts.with_timezone(&Local).date_naive().num_days_from_ce()
}

fn add_event(data: &mut FileData, ts: DateTime<Utc>, model: &str, cost: f64, tokens: f64) {
    let entry = data
        .days
        .entry((day_of_utc(ts), model.to_string()))
        .or_insert((0.0, 0.0));
    entry.0 += cost;
    entry.1 += tokens;
}

fn note_unpriced(data: &mut FileData, model: &str) {
    *data.unpriced.entry(model.to_string()).or_insert(0) += 1;
}

fn merge_data(target: &mut FileData, source: FileData) {
    for (key, (cost, tokens)) in source.days {
        let entry = target.days.entry(key).or_insert((0.0, 0.0));
        entry.0 += cost;
        entry.1 += tokens;
    }
    for (model, count) in source.unpriced {
        *target.unpriced.entry(model).or_insert(0) += count;
    }
}

/// Ranked model list for one window: top models by cost, anything past the
/// fifth name or under a 5% share folds into "Other".
fn finalize_models(raw: HashMap<String, (f64, f64)>, window_cost: f64) -> Vec<ModelSpend> {
    let mut list: Vec<ModelSpend> = raw
        .into_iter()
        .map(|(model, (cost, tokens))| ModelSpend { model, cost, tokens })
        .collect();
    list.sort_by(|a, b| b.cost.partial_cmp(&a.cost).unwrap_or(std::cmp::Ordering::Equal));

    let mut named = Vec::new();
    let mut other = ModelSpend { model: "Other".into(), cost: 0.0, tokens: 0.0 };
    for (i, m) in list.into_iter().enumerate() {
        let share = if window_cost > 0.0 { m.cost / window_cost } else { 0.0 };
        if i < 5 && (share >= 0.05 || i == 0) {
            named.push(m);
        } else {
            other.cost += m.cost;
            other.tokens += m.tokens;
        }
    }
    if other.cost > 0.001 || other.tokens > 0.0 {
        named.push(other);
    }
    named
}

fn build_spend(id: &'static str, name: &'static str, data: FileData) -> ProviderSpend {
    let today = Local::now().date_naive().num_days_from_ce();
    let mut unpriced_models: Vec<String> = data.unpriced.keys().cloned().collect();
    unpriced_models.sort();
    unpriced_models.truncate(5);
    let days = data.days;
    let mut sp = ProviderSpend {
        id,
        name,
        today: Window::default(),
        yesterday: Window::default(),
        last30: Window::default(),
        trend: vec![0.0; TREND_DAYS],
        unpriced: data.unpriced.values().sum(),
        unpriced_models,
    };
    let mut models: [HashMap<String, (f64, f64)>; 3] =
        [HashMap::new(), HashMap::new(), HashMap::new()];

    for ((day, model), (cost, tokens)) in days {
        let mut bump = |idx: usize, w: &mut Window| {
            w.cost += cost;
            w.tokens += tokens;
            let entry = models[idx].entry(model.clone()).or_insert((0.0, 0.0));
            entry.0 += cost;
            entry.1 += tokens;
        };
        if day == today {
            bump(0, &mut sp.today);
        }
        if day == today - 1 {
            bump(1, &mut sp.yesterday);
        }
        if day > today - TREND_DAYS as i32 {
            bump(2, &mut sp.last30);
            let idx = (day - (today - TREND_DAYS as i32 + 1)) as usize;
            if idx < TREND_DAYS {
                sp.trend[idx] += tokens;
            }
        }
    }

    let [m0, m1, m2] = models;
    sp.today.models = finalize_models(m0, sp.today.cost);
    sp.yesterday.models = finalize_models(m1, sp.yesterday.cost);
    sp.last30.models = finalize_models(m2, sp.last30.cost);
    sp
}

/// All .jsonl files under `root` modified in the last 31 days.
fn recent_jsonl_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else { return };
    let cutoff = SystemTime::now() - Duration::from_secs(31 * 86_400);
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            recent_jsonl_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            if let Ok(meta) = entry.metadata() {
                if meta.modified().map(|m| m >= cutoff).unwrap_or(true) {
                    out.push(path);
                }
            }
        }
    }
}

/// Parses one file into per-day totals, via the cache when unchanged.
fn file_days(path: &Path, parse: &mut dyn FnMut(&str, &mut FileData)) -> FileData {
    let Ok(meta) = fs::metadata(path) else { return FileData::default() };
    let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let size = meta.len();

    let gen = pricing::generation();
    if let Ok(map) = cache().lock() {
        if let Some(entry) = map.get(path) {
            if entry.mtime == mtime && entry.size == size && entry.gen == gen {
                return entry.data.clone();
            }
        }
    }

    let mut data = FileData::default();
    if let Ok(file) = fs::File::open(path) {
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            parse(&line, &mut data);
        }
    }

    if let Ok(mut map) = cache().lock() {
        map.insert(path.to_path_buf(), FileEntry { mtime, size, gen, data: data.clone() });
    }
    data
}

fn parse_ts(value: Option<&Value>) -> Option<DateTime<Utc>> {
    let value = value?;
    if let Some(s) = value.as_str() {
        return DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc));
    }
    if let Some(n) = value.as_i64() {
        // Heuristic: values past ~2001-09 in ms are millisecond stamps.
        let ms = if n > 1_000_000_000_000 { n } else { n * 1000 };
        return DateTime::from_timestamp_millis(ms);
    }
    None
}

// ---------------------------------------------------------------------------
// Pricing (dollars per million tokens: input, output, cache read, cache write)
// ---------------------------------------------------------------------------

fn claude_price(model: &str) -> Option<(f64, f64, f64, f64)> {
    let m = model.to_lowercase();
    if m.contains("opus") {
        Some((15.0, 75.0, 1.5, 18.75))
    } else if m.contains("sonnet") {
        Some((3.0, 15.0, 0.3, 3.75))
    } else if m.contains("haiku") {
        Some((1.0, 5.0, 0.1, 1.25))
    } else {
        // Unknown model (or a new family): rely on the log's own costUSD;
        // counting tokens at a guessed price would fabricate dollars.
        None
    }
}

fn codex_price(model: &str) -> (f64, f64, f64) {
    let m = model.to_lowercase();
    if m.contains("mini") || m.contains("spark") {
        (0.25, 2.0, 0.025)
    } else {
        // gpt-5 family / codex defaults
        (1.25, 10.0, 0.125)
    }
}

fn grok_price(model: &str) -> (f64, f64) {
    let m = model.to_lowercase();
    if m.contains("code") || m.contains("fast") {
        (0.2, 1.5)
    } else {
        (3.0, 15.0)
    }
}

// ---------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------

/// Claude Code writes one JSONL per session under ~/.claude/projects. Each
/// assistant line carries usage token counts and usually a precomputed
/// costUSD, which we prefer over our own pricing table.
fn claude() -> ProviderSpend {
    let root = std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".claude"))
        .join("projects");

    let mut files = Vec::new();
    recent_jsonl_files(&root, &mut files);
    let mut all = FileData::default();
    for file in files {
        // Resumed sessions can repeat messages within a file; dedupe on
        // message id + request id.
        let mut seen: HashSet<String> = HashSet::new();
        let data = file_days(&file, &mut |line, data| {
            if !line.contains("\"type\":\"assistant\"") {
                return;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else { return };
            if v.get("type").and_then(Value::as_str) != Some("assistant") {
                return;
            }
            let Some(ts) = parse_ts(v.get("timestamp")) else { return };

            if let (Some(mid), Some(rid)) = (
                v.pointer("/message/id").and_then(Value::as_str),
                v.get("requestId").and_then(Value::as_str),
            ) {
                if !seen.insert(format!("{mid}:{rid}")) {
                    return;
                }
            }

            let model = v
                .pointer("/message/model")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let usage = v.pointer("/message/usage").cloned().unwrap_or(Value::Null);
            let num = |k: &str| usage.get(k).and_then(Value::as_f64).unwrap_or(0.0);
            let input = num("input_tokens");
            let output = num("output_tokens");
            let cache_read = num("cache_read_input_tokens");
            let cache_write = num("cache_creation_input_tokens");
            let tokens = input + output + cache_read + cache_write;

            // Cost preference: the log's own costUSD → live catalog price
            // → static family fallback → excluded (never a guessed $0).
            let cost = match v.get("costUSD").and_then(Value::as_f64) {
                Some(c) => c,
                None => {
                    let rate = pricing::lookup(&model)
                        .map(|p| (p.input, p.output, p.cache_read, p.cache_write))
                        .or_else(|| claude_price(&model));
                    match rate {
                        Some((i, o, cr, cw)) => {
                            (input * i + output * o + cache_read * cr + cache_write * cw) / 1e6
                        }
                        None => {
                            if tokens > 0.0 {
                                note_unpriced(data, &model);
                            }
                            return;
                        }
                    }
                }
            };

            if tokens > 0.0 || cost > 0.0 {
                add_event(data, ts, &model, cost, tokens);
            }
        });
        merge_data(&mut all, data);
    }
    build_spend("claude", "Claude", all)
}

/// Codex rollout files log a token_count event per turn; the model rides in
/// the surrounding turn_context/session_meta lines.
fn codex() -> ProviderSpend {
    let home = std::env::var("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".codex"));

    // An archived session is often a byte-for-byte copy of one still in
    // sessions/ — count each relative path once, sessions/ winning.
    let sessions_root = home.join("sessions");
    let archived_root = home.join("archived_sessions");
    let mut files = Vec::new();
    recent_jsonl_files(&sessions_root, &mut files);
    let live_rel: HashSet<PathBuf> = files
        .iter()
        .filter_map(|f| f.strip_prefix(&sessions_root).ok().map(Path::to_path_buf))
        .collect();
    let mut archived = Vec::new();
    recent_jsonl_files(&archived_root, &mut archived);
    files.extend(archived.into_iter().filter(|f| {
        f.strip_prefix(&archived_root)
            .map(|rel| !live_rel.contains(rel))
            .unwrap_or(true)
    }));

    let mut all = FileData::default();
    for file in files {
        let mut model = String::from("gpt-5");
        let data = file_days(&file, &mut |line, data| {
            if !(line.contains("token_count")
                || line.contains("turn_context")
                || line.contains("session_meta"))
            {
                return;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else { return };
            if let Some(m) = v.pointer("/payload/model").and_then(Value::as_str) {
                model = m.to_string();
                return;
            }
            if v.pointer("/payload/type").and_then(Value::as_str) != Some("token_count") {
                return;
            }
            let Some(ts) = parse_ts(v.get("timestamp")) else { return };
            let usage = v
                .pointer("/payload/info/last_token_usage")
                .cloned()
                .unwrap_or(Value::Null);
            let num = |k: &str| usage.get(k).and_then(Value::as_f64).unwrap_or(0.0);
            let input = num("input_tokens");
            let cached = num("cached_input_tokens").min(input);
            let output = num("output_tokens");
            let tokens = input + output;
            if tokens <= 0.0 {
                return;
            }
            // Live catalog first; the static gpt-5 table only for models
            // that are recognizably Codex-family; anything else is excluded.
            let lower = model.to_lowercase();
            let rate = pricing::lookup(&model).map(|p| (p.input, p.output, p.cache_read));
            let (pi, po, pc) = match rate {
                Some(r) => r,
                None if lower.contains("gpt") || lower.contains("codex") => codex_price(&model),
                None => {
                    note_unpriced(data, &model);
                    return;
                }
            };
            let cost = ((input - cached) * pi + cached * pc + output * po) / 1e6;
            add_event(data, ts, &model, cost, tokens);
        });
        merge_data(&mut all, data);
    }
    build_spend("codex", "Codex", all)
}

/// Grok CLI appends one global log at ~/.grok/logs/unified.jsonl (or under
/// $GROK_HOME). Token counts ride on `shell.turn.inference_done` lines
/// (prompt/completion/reasoning/cached_prompt); those rows carry no model
/// id, so the active model is tracked per CLI process from the model-change
/// events the CLI also logs — the same scheme the Mac scanner uses.
fn grok() -> ProviderSpend {
    let root = std::env::var("GROK_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".grok"));
    let path = root.join("logs").join("unified.jsonl");
    let mut all = FileData::default();
    if path.exists() {
        let mut model_by_pid: HashMap<i64, String> = HashMap::new();
        let data = file_days(&path, &mut |line, data| {
            if !line.contains("inference_done") && !line.contains("model") {
                return;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else { return };
            let Some(msg) = v.get("msg").and_then(Value::as_str) else { return };
            let ctx = v.get("ctx").cloned().unwrap_or(Value::Null);
            let pid = v.get("pid").and_then(Value::as_i64);
            let model_field = match msg {
                "model changed" => ctx.get("model"),
                "model catalog: notifying clients" => ctx.get("current_model_id"),
                "backend_search: model switch" => ctx
                    .get("model")
                    .or_else(|| ctx.get("current_model_id"))
                    .or_else(|| ctx.get("model_id")),
                "subagent model resolved" => ctx.get("model_id").or_else(|| ctx.get("model")),
                _ => None,
            };
            if let Some(m) = model_field.and_then(Value::as_str) {
                let m = m.trim();
                if !m.is_empty() {
                    if let Some(pid) = pid {
                        model_by_pid.insert(pid, m.to_string());
                    }
                }
                return;
            }
            if msg != "shell.turn.inference_done" {
                return;
            }
            let num = |k: &str| ctx.get(k).and_then(Value::as_f64);
            let Some(prompt) = num("prompt_tokens") else { return };
            let Some(ts) = parse_ts(v.get("ts")) else { return };
            let output = num("completion_tokens").unwrap_or(0.0) + num("reasoning_tokens").unwrap_or(0.0);
            // cached_prompt_tokens is a subset of prompt_tokens, so the total
            // counts the prompt once.
            let cached = num("cached_prompt_tokens").unwrap_or(0.0).min(prompt);
            let tokens = prompt + output;
            if tokens <= 0.0 {
                return;
            }
            // Token rows carry no model id — attribute via the row's process;
            // rows with no attributable model are excluded, like the Mac.
            let Some(model) = pid.and_then(|p| model_by_pid.get(&p)).cloned() else { return };
            let rate = pricing::lookup(&model).map(|p| (p.input, p.output, p.cache_read));
            let (pi, po, pc) = match rate {
                Some(r) => r,
                // Static backstop only for recognizably Grok-family models
                // (catalog down); it has no cache rate, so cached tokens are
                // conservatively priced as fresh input there.
                None if model.to_lowercase().contains("grok") => {
                    let (i, o) = grok_price(&model);
                    (i, o, i)
                }
                None => {
                    note_unpriced(data, &model);
                    return;
                }
            };
            let cost = ((prompt - cached) * pi + cached * pc + output * po) / 1e6;
            add_event(data, ts, &model, cost, tokens);
        });
        merge_data(&mut all, data);
    }
    build_spend("grok", "Grok", all)
}

/// OpenCode stores real per-message costs in its database — no pricing
/// table needed.
fn opencode() -> ProviderSpend {
    let mut data = FileData::default();
    for (ts_ms, cost, tokens, model) in providers::opencode::collect_cost_events() {
        if let Some(ts) = DateTime::from_timestamp_millis(ts_ms as i64) {
            add_event(&mut data, ts, &model, cost, tokens);
        }
    }
    build_spend("opencode", "OpenCode", data)
}

/// Cursor spend from the dashboard's usage-events CSV export (fetched by the
/// async caller — this stays a pure parser). Column layout is discovered
/// from the header row; rows with an explicit cost win, token-only rows are
/// priced via the live catalog (the supplement carries Cursor-native models).
pub fn cursor_from_csv(csv: &str) -> ProviderSpend {
    let mut data = FileData::default();
    let mut lines = csv.lines();
    let Some(header) = lines.next() else {
        return build_spend("cursor", "Cursor", data);
    };
    let cols: Vec<String> = split_csv_row(header)
        .into_iter()
        .map(|c| c.trim().to_lowercase())
        .collect();
    let find = |names: &[&str]| {
        cols.iter().position(|c| names.iter().any(|n| c.contains(n)))
    };
    let date_col = find(&["date", "time"]);
    let model_col = find(&["model"]);
    let cost_col = find(&["cost", "amount", "price"]);
    // "Input (w/ Cache Write)" is write-inclusive; the w/o column is the
    // plain input. Their difference gets the cache-write rate.
    let input_wo_col = cols.iter().position(|c| c.contains("input") && c.contains("w/o"));
    let input_with_col = cols
        .iter()
        .position(|c| c.contains("input") && !c.contains("w/o"));
    let output_col = find(&["output"]);
    let cache_read_col = find(&["cache read", "cache_read", "cacheread"]);
    let total_col = find(&["total tokens", "total_tokens"]);
    let (Some(date_col), Some(model_col)) = (date_col, model_col) else {
        return build_spend("cursor", "Cursor", data);
    };

    for line in lines {
        let row = split_csv_row(line);
        let get = |i: Option<usize>| i.and_then(|i| row.get(i)).map(|s| s.trim()).unwrap_or("");
        let Some(ts) = parse_csv_date(get(Some(date_col))) else { continue };
        let model = {
            let m = get(Some(model_col));
            if m.is_empty() { "Unattributed".to_string() } else { m.to_string() }
        };
        let num = |i: Option<usize>| {
            get(i).replace(['$', ','], "").parse::<f64>().unwrap_or(0.0)
        };
        let input_with = num(input_with_col);
        let input_wo = if input_wo_col.is_some() { num(input_wo_col) } else { input_with };
        let cache_write = (input_with - input_wo).max(0.0);
        let output = num(output_col);
        let cache_read = num(cache_read_col);
        let tokens = {
            let t = num(total_col);
            if t > 0.0 { t } else { input_with + output + cache_read }
        };
        let explicit_cost = num(cost_col);

        if explicit_cost > 0.0 {
            add_event(&mut data, ts, &model, explicit_cost, tokens);
        } else if tokens > 0.0 {
            match pricing::lookup(&model) {
                Some(p) => {
                    let cost = (input_wo * p.input
                        + cache_write * p.cache_write
                        + output * p.output
                        + cache_read * p.cache_read)
                        / 1e6;
                    add_event(&mut data, ts, &model, cost, tokens);
                }
                None => note_unpriced(&mut data, &model),
            }
        }
    }
    build_spend("cursor", "Cursor", data)
}

/// Cursor CSV dates arrive in several shapes depending on export era:
/// RFC3339, "YYYY-MM-DD HH:MM:SS", bare "YYYY-MM-DD", or epoch (s/ms).
fn parse_csv_date(s: &str) -> Option<DateTime<Utc>> {
    if s.is_empty() {
        return None;
    }
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Some(d.with_timezone(&Utc));
    }
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f", "%m/%d/%Y %H:%M:%S", "%b %d, %Y, %I:%M %p", "%b %d, %Y"] {
        if let Ok(d) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(d.and_utc());
        }
        if let Ok(d) = chrono::NaiveDate::parse_from_str(s, fmt) {
            return d.and_hms_opt(12, 0, 0).map(|dt| dt.and_utc());
        }
    }
    if let Ok(n) = s.parse::<i64>() {
        return DateTime::from_timestamp_millis(if n > 1_000_000_000_000 { n } else { n * 1000 });
    }
    None
}

#[cfg(test)]
mod tests {
    /// Live probe over this machine's real logs + Cursor export. Prints
    /// aggregates and the CSV header only. Run via
    /// `cargo test --lib spend -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_probe() {
        let csv = tauri::async_runtime::block_on(crate::providers::cursor::fetch_usage_csv());
        eprintln!("cursor csv: {} bytes", csv.as_deref().map(str::len).unwrap_or(0));
        if let Some(c) = &csv {
            eprintln!("csv header: {}", c.lines().next().unwrap_or(""));
            for row in c.lines().skip(1).take(3) {
                let cells = super::split_csv_row(row);
                eprintln!(
                    "row: date={:?} parsed={} model={:?} in={:?} out={:?} total={:?} cost={:?}",
                    cells.first(),
                    cells.first().map(|d| super::parse_csv_date(d).is_some()).unwrap_or(false),
                    cells.get(4),
                    cells.get(6),
                    cells.get(9),
                    cells.get(10),
                    cells.get(11),
                );
            }
        }
        for sp in super::collect(csv) {
            eprintln!(
                "{}: today=${:.2} 30d=${:.2} tokens30={:.0} unpriced={} {:?}",
                sp.id, sp.today.cost, sp.last30.cost, sp.last30.tokens, sp.unpriced, sp.unpriced_models
            );
        }
    }
}

/// Minimal CSV field splitter with quoted-field support.
fn split_csv_row(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => out.push(std::mem::take(&mut field)),
            _ => field.push(c),
        }
    }
    out.push(field);
    out
}

pub fn collect(cursor_csv: Option<String>) -> Vec<ProviderSpend> {
    pricing::ensure_fresh();
    let mut list = vec![claude(), codex(), grok(), opencode()];
    if let Some(csv) = cursor_csv {
        list.push(cursor_from_csv(&csv));
    }
    // Models nothing prices yet (new slugs ship often): flag the catalog
    // to look for updates hourly instead of daily.
    if list.iter().any(|sp| sp.unpriced > 0) {
        pricing::note_unpriced();
    }
    list.into_iter().filter(ProviderSpend::has_data).collect()
}
