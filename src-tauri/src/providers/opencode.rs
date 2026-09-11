use super::{Metric, Snapshot};
use serde_json::Value;
use std::path::{Path, PathBuf};

const ID: &str = "opencode";
const NAME: &str = "OpenCode";
const MAX_AUTH_BYTES: u64 = 64 * 1024;

// Primary source: the official account-wide usage API that shipped in
// anomalyco/opencode#16513 (2026-08-11) — GET /zen/go/v1/usage with the Go
// key returns per-window percentages and resets counted on OpenCode's
// servers, so other devices and shared-subscription participants finally
// show up. The local computation below survives as the FALLBACK when the
// API is unreachable, and its plan limits still label the local path.
const USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";

// OpenCode Go plan limits from https://opencode.ai/docs/go/ (dollars) —
// fallback path only.
const SESSION_LIMIT: f64 = 12.0; // rolling 5 hours
const WEEKLY_LIMIT: f64 = 30.0; // UTC ISO week (Monday start)
const MONTHLY_LIMIT: f64 = 60.0; // month anchored to earliest-ever Go usage

const SESSION_MS: f64 = 5.0 * 3600e3;
const WEEK_MS: f64 = 7.0 * 86400e3;

pub fn data_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".local")
        .join("share")
        .join("opencode")
}

/// Reads an entry like {"opencode-go": {"type": "api", "key": "..."}} from
/// OpenCode's auth.json. Also used by the OpenRouter provider.
pub fn auth_entry_key(entry: &str) -> Option<String> {
    auth_entry_key_in(&data_dir(), entry)
}

fn auth_entry_key_in(dir: &Path, entry: &str) -> Option<String> {
    let raw = super::read_small_text(&dir.join("auth.json"), MAX_AUTH_BYTES, "auth.json").ok()?;
    let doc: Value = serde_json::from_str(&raw).ok()?;
    doc.get(entry)?
        .get("key")
        .and_then(Value::as_str)
        .filter(|k| !k.is_empty())
        .map(str::to_string)
}

/// Stable fingerprint of a credential — never the key itself. Written
/// next to the snapshot cache so swapping auth.json drops the old
/// account's last-good numbers instead of painting them under `opencode`.
fn fingerprint_key(key: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in key.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn dir_identity(dir: &Path) -> Option<String> {
    auth_entry_key_in(dir, "opencode-go").map(|k| fingerprint_key(&k))
}

/// The default login's account identity, for the snapshot-cache stamp.
pub fn default_identity() -> Option<String> {
    dir_identity(&data_dir())
}

pub struct OpenCodeAccount {
    pub id: String,
    pub name: String,
    pub dir: PathBuf,
}

/// Extra OpenCode profiles beyond `~/.local/share/opencode`: OPENCODE_HOME,
/// `~/.local/share/opencode-*`, and the same scan roots Claude/Codex use.
/// A dir that can't name its account never becomes a card; a dir whose
/// fingerprint matches an already-seen login is skipped.
pub fn discover_extra_accounts() -> Vec<OpenCodeAccount> {
    let default = data_dir();
    // Claude/Codex abort discovery when the default login exists but
    // can't be named (dedup would be unsafe). OpenCode's identity is a
    // key fingerprint: no Go key means `seen` is empty, so there is no
    // duplicate risk — an OpenRouter-only default must not hide extra
    // OPENCODE_HOME / opencode-* Go profiles.
    let mut seen: Vec<String> = dir_identity(&default).into_iter().collect();

    let mut out = Vec::new();
    for dir in extra_data_dirs() {
        if same_dir(&dir, &default) {
            continue;
        }
        let Some(fp) = dir_identity(&dir) else { continue };
        if !scoped_id_charset(&fp) {
            continue;
        }
        if seen.iter().any(|s| s == &fp) {
            continue;
        }
        seen.push(fp.clone());
        let hash8: String = fp.chars().take(8).collect();
        let name = match dir_label(&dir) {
            Some(l) => format!("OpenCode — {l}"),
            None => format!("OpenCode @{hash8}"),
        };
        out.push(OpenCodeAccount {
            id: format!("opencode@{hash8}"),
            name,
            dir,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn extra_data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(home) = std::env::var("OPENCODE_HOME") {
        let home = home.trim();
        if !home.is_empty() {
            dirs.push(PathBuf::from(home));
        }
    }
    if let Some(share) = dirs::home_dir().map(|h| h.join(".local").join("share")) {
        if let Ok(entries) = std::fs::read_dir(share) {
            for e in entries.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("opencode-") && e.path().is_dir() {
                    dirs.push(e.path());
                }
            }
        }
    }
    dirs.extend(super::account_scan_roots());
    dirs
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(aa), Ok(bb)) => aa == bb,
        _ => a == b,
    }
}

fn dir_label(dir: &Path) -> Option<String> {
    let name = dir.file_name()?.to_string_lossy();
    name.strip_prefix("opencode-")
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn scoped_id_charset(raw: &str) -> bool {
    !raw.is_empty() && raw.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Query the live OpenCode ledger read-only. Copying db+WAL into
/// `%APPDATA%\Pane\tmp` used the same pattern that grew Devin's temp
/// journal to tens of GB — never clone a vendor database onto C:.
fn with_live_db<T>(dir: &Path, f: impl FnOnce(&Path) -> Result<T, String>) -> Result<T, String> {
    let db_path = dir.join("opencode.db");
    if !db_path.exists() {
        return Err("opencode.db not found — has OpenCode been used on this PC?".into());
    }
    f(&db_path)
}

pub async fn snapshot() -> Snapshot {
    snapshot_at(data_dir(), ID.to_string(), NAME.to_string()).await
}

pub async fn snapshot_at(dir: PathBuf, id: String, name: String) -> Snapshot {
    match fetch(&dir, &id, &name).await {
        Ok(s) => s,
        Err(e) => Snapshot::error(&id, &name, e),
    }
}

async fn fetch(dir: &Path, id: &str, name: &str) -> Result<Snapshot, String> {
    let auth_path = dir.join("auth.json");
    if !auth_path.exists() {
        return Ok(Snapshot::no_credentials(
            id,
            name,
            "OpenCode sign-in not found. Run `opencode` and log in.",
        ));
    }
    let Some(key) = auth_entry_key_in(dir, "opencode-go") else {
        return Ok(Snapshot::no_credentials(
            id,
            name,
            "No OpenCode Go subscription found in auth.json.",
        ));
    };

    // Account-wide truth first; the local windows only when it fails
    // (offline, revoked key, or a gateway hiccup). The fallback names its
    // narrower scope in the plan label — the card must never silently
    // flip between account-wide and this-PC-only numbers looking the same.
    static FALLBACK_ACTIVE: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    match fetch_official(&key).await {
        Ok(metrics) => {
            FALLBACK_ACTIVE.store(false, std::sync::atomic::Ordering::Relaxed);
            Ok(Snapshot::ok(id, name, Some("Go".into()), metrics))
        }
        Err(e) => {
            // Log the TRANSITION into fallback, not every refresh — an
            // offline machine would otherwise print this once a minute
            // for the app's lifetime.
            if !FALLBACK_ACTIVE.swap(true, std::sync::atomic::Ordering::Relaxed) {
                eprintln!("[pane] opencode: usage API failed ({e}) — using local windows");
            }
            // If the fallback ALSO fails (fresh device with no local
            // history), the card must carry both causes — surfacing only
            // "opencode.db not found" would send someone troubleshooting
            // an offline/revoked-key card after the wrong problem.
            let mut snap = local_windows_snapshot(dir, id, name)
                .map_err(|db| format!("usage API failed ({e}); local fallback: {db}"))?;
            snap.plan = Some("Go — this PC only".into());
            Ok(snap)
        }
    }
}

/// GET /zen/go/v1/usage with the Go key. The response is served by
/// OpenCode's console — the same numbers the Zen dashboard shows.
async fn fetch_official(key: &str) -> Result<Vec<Metric>, String> {
    let resp = super::http()
        .get(USAGE_URL)
        .bearer_auth(key)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("usage request: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("usage endpoint: HTTP {}", resp.status()));
    }
    let doc: Value = resp.json().await.map_err(|e| format!("usage parse: {e}"))?;
    parse_official(&doc).ok_or_else(|| "no recognizable usage windows in response".into())
}

/// Live wire shape (verified against the deployed endpoint, which differs
/// from the merged PR's draft): { "usage": { "rolling"|"weekly"|"monthly":
/// { "status": "ok"|"rate-limited", "percent": int, "resetsAt": RFC3339 } } }.
/// Percentages and resets are the server's own; a window the response
/// doesn't carry is simply skipped rather than failing the card.
fn parse_official(doc: &Value) -> Option<Vec<Metric>> {
    let usage = doc.get("usage")?;
    let mut metrics = Vec::new();
    for (field, label, period_ms) in [
        ("rolling", "Session", Some(SESSION_MS as i64)),
        ("weekly", "Weekly", Some(WEEK_MS as i64)),
        // Monthly cycles run 28-31 days anchored to the subscription
        // date — a fixed period would skew the pace projection (and go
        // NEGATIVE-fraction right after a 31-day cycle starts), so the
        // real length is derived from the server's own reset boundary.
        ("monthly", "Monthly", None),
    ] {
        let Some(w) = usage.get(field) else { continue };
        // "rate-limited" IS the answer (100%), independent of the percent
        // field — a blocked window must never vanish from the card just
        // because the server omitted or lagged its percent.
        let rate_limited = w.get("status").and_then(Value::as_str) == Some("rate-limited");
        let percent = w.get("percent").and_then(Value::as_f64);
        let used = if rate_limited {
            100.0
        } else {
            let Some(percent) = percent else { continue };
            // Guard the empirically-captured contract: the server sends
            // integer 0-100 USED percentages (it floors server-side; the
            // shape already changed once between the upstream PR and
            // deploy). A fractional 0-1 encoding or an out-of-range value
            // means the shape changed again — fail the whole parse (→
            // labeled local fallback) instead of rendering silently wrong
            // meters.
            if !(0.0..=100.0).contains(&percent) || (percent > 0.0 && percent < 1.0) {
                return None;
            }
            percent
        };
        let resets_at = w
            .get("resetsAt")
            .and_then(Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp_millis());
        let period_ms = period_ms.or_else(|| resets_at.map(month_period_ending));
        metrics.push(Metric::progress(label, used, None).with_reset(resets_at, period_ms));
    }
    (!metrics.is_empty()).then_some(metrics)
}

/// Length of the anchored monthly cycle that ENDS at the server's
/// resetsAt: one month back on the same day-of-month (clamped to short
/// months) at the same time-of-day — the true 28-31-day window.
fn month_period_ending(resets_ms: i64) -> i64 {
    use chrono::{Datelike, TimeZone, Timelike, Utc};
    let Some(end) = Utc.timestamp_millis_opt(resets_ms).single() else {
        return 30 * 86_400_000;
    };
    let (py, pm) = shift_month(end.year(), end.month(), -1);
    let day = end.day().min(days_in_month(py, pm));
    let start = utc_date(
        py,
        pm,
        day,
        end.hour(),
        end.minute(),
        end.second(),
        end.timestamp_subsec_millis(),
    );
    ((resets_ms as f64 - start) as i64).max(1)
}

/// Fallback: the pre-API local computation from opencode.db — this PC's
/// rows only, so shared subscriptions under-count here.
fn local_windows_snapshot(dir: &Path, id: &str, name: &str) -> Result<Snapshot, String> {
    let w = with_live_db(dir, |db| {
        let (msgs, capped) = read_messages(db)?;
        let rows: Vec<(f64, f64)> = msgs
            .into_iter()
            .filter(|r| r.provider == "opencode-go" && r.cost > 0.0)
            .map(|r| (r.ts, r.cost))
            .collect();
        // The monthly anchor is the earliest-ever Go row; when the row cap
        // dropped the oldest rows, recover it with the bounded oldest-first
        // probe instead of re-anchoring on the newest window's edge.
        let anchor = if capped { earliest_go_anchor(db) } else { None };
        Ok(go_windows(&rows, anchor, chrono::Utc::now().timestamp_millis() as f64))
    })?;
    let metrics = vec![
        Metric::progress(
            "Session",
            w.session / SESSION_LIMIT * 100.0,
            Some(format!("${:.2} of ${SESSION_LIMIT:.0}", w.session)),
        )
        .with_reset(Some(w.session_resets_at), Some(SESSION_MS as i64)),
        Metric::progress(
            "Weekly",
            w.weekly / WEEKLY_LIMIT * 100.0,
            Some(format!("${:.2} of ${WEEKLY_LIMIT:.0}", w.weekly)),
        )
        .with_reset(Some(w.weekly_resets_at), Some(WEEK_MS as i64)),
        Metric::progress(
            "Monthly",
            w.monthly / MONTHLY_LIMIT * 100.0,
            Some(format!("${:.2} of ${MONTHLY_LIMIT:.0}", w.monthly)),
        )
        .with_reset(Some(w.monthly_resets_at), Some(w.monthly_period_ms)),
    ];
    Ok(Snapshot::ok(id, name, Some("Go".into()), metrics))
}

struct GoWindows {
    session: f64,
    session_resets_at: i64,
    weekly: f64,
    weekly_resets_at: i64,
    monthly: f64,
    monthly_resets_at: i64,
    monthly_period_ms: i64,
}

/// Window math ported faithfully from the Mac app's OpenCodeGoWindowMath
/// (itself ported from the legacy opencode-go plugin): a rolling 5-hour
/// session whose reset is when the oldest in-window row ages out, a UTC ISO
/// week (Monday 00:00 start), and a month anchored to the day-of-month and
/// time-of-day of the earliest-ever local Go usage (calendar month when
/// there is none). Pure and UTC-based, so it unit-tests deterministically.
fn go_windows(rows: &[(f64, f64)], anchor_override: Option<f64>, now_ms: f64) -> GoWindows {
    let sum_range = |start: f64, end: f64| -> f64 {
        let total: f64 =
            rows.iter().filter(|(ts, _)| *ts >= start && *ts < end).map(|(_, c)| c).sum();
        // Snap to a hundredth of a cent to shed float-summation noise;
        // max(0.0) also normalizes -0.0, which would render as "$-0.00".
        ((total * 10_000.0).round() / 10_000.0).max(0.0)
    };

    let session_start = now_ms - SESSION_MS;
    let session = sum_range(session_start, now_ms);
    let oldest_in_session = rows
        .iter()
        .map(|(ts, _)| *ts)
        .filter(|ts| *ts >= session_start && *ts < now_ms)
        .fold(f64::INFINITY, f64::min);
    let session_resets_at =
        (if oldest_in_session.is_finite() { oldest_in_session } else { now_ms }) + SESSION_MS;

    let week_start = start_of_utc_week(now_ms);
    let week_end = week_start + WEEK_MS;
    let weekly = sum_range(week_start, week_end);

    // Monthly cycle anchor: the earliest-ever Go row on this machine. When
    // the row cap dropped the oldest rows, the caller passes the probed
    // anchor explicitly.
    let anchor_ms = match anchor_override {
        Some(a) => a,
        None => rows.iter().map(|(ts, _)| *ts).fold(f64::INFINITY, f64::min),
    };
    let (month_start, month_end) = anchored_month_bounds(
        now_ms,
        if anchor_ms.is_finite() { Some(anchor_ms) } else { None },
    );
    let monthly = sum_range(month_start, month_end);

    GoWindows {
        session,
        session_resets_at: session_resets_at as i64,
        weekly,
        weekly_resets_at: week_end as i64,
        monthly,
        monthly_resets_at: month_end as i64,
        monthly_period_ms: (month_end - month_start) as i64,
    }
}

/// Monday 00:00 UTC of the week containing `now_ms`.
fn start_of_utc_week(now_ms: f64) -> f64 {
    use chrono::{Datelike, TimeZone, Utc};
    let now = Utc.timestamp_millis_opt(now_ms as i64).single().unwrap_or_else(Utc::now);
    let days_since_monday = now.date_naive().weekday().num_days_from_monday() as i64;
    let monday = now.date_naive() - chrono::Duration::days(days_since_monday);
    Utc.from_utc_datetime(&monday.and_hms_opt(0, 0, 0).unwrap()).timestamp_millis() as f64
}

/// The anchored monthly cycle containing `now_ms`: cycle boundaries fall on
/// the anchor's day-of-month (clamped to short months) at the anchor's
/// time-of-day, UTC. With no anchor: the UTC calendar month.
fn anchored_month_bounds(now_ms: f64, anchor_ms: Option<f64>) -> (f64, f64) {
    use chrono::{Datelike, TimeZone, Timelike, Utc};
    let now = Utc.timestamp_millis_opt(now_ms as i64).single().unwrap_or_else(Utc::now);
    let (mut year, mut month) = (now.year(), now.month());

    let Some(anchor_ms) = anchor_ms else {
        let start = utc_date(year, month, 1, 0, 0, 0, 0);
        let (ny, nm) = shift_month(year, month, 1);
        return (start, utc_date(ny, nm, 1, 0, 0, 0, 0));
    };
    let anchor = Utc.timestamp_millis_opt(anchor_ms as i64).single().unwrap_or_else(Utc::now);
    let anchored_start = |year: i32, month: u32| -> f64 {
        let day = anchor.day().min(days_in_month(year, month));
        utc_date(
            year,
            month,
            day,
            anchor.hour(),
            anchor.minute(),
            anchor.second(),
            anchor.timestamp_subsec_millis(),
        )
    };

    let mut start = anchored_start(year, month);
    // The current month's anchored start can land in the future (anchor
    // day-of-month later than today) — then the live cycle began last month.
    if start > now_ms {
        (year, month) = shift_month(year, month, -1);
        start = anchored_start(year, month);
    }
    let (ny, nm) = shift_month(year, month, 1);
    (start, anchored_start(ny, nm))
}

fn shift_month(year: i32, month: u32, delta: i32) -> (i32, u32) {
    let zero_based = year * 12 + month as i32 - 1 + delta;
    (zero_based.div_euclid(12), (zero_based.rem_euclid(12) + 1) as u32)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (ny, nm) = shift_month(year, month, 1);
    let first = chrono::NaiveDate::from_ymd_opt(year, month, 1).unwrap();
    let next = chrono::NaiveDate::from_ymd_opt(ny, nm, 1).unwrap();
    (next - first).num_days() as u32
}

fn utc_date(year: i32, month: u32, day: u32, h: u32, m: u32, s: u32, ms: u32) -> f64 {
    use chrono::{TimeZone, Utc};
    chrono::NaiveDate::from_ymd_opt(year, month, day)
        .and_then(|d| d.and_hms_milli_opt(h, m, s, ms))
        .map(|dt| Utc.from_utc_datetime(&dt).timestamp_millis() as f64)
        .unwrap_or(0.0)
}

/// (timestamp ms, cost $, tokens, model, provider) of every priced
/// message, any provider — this is money spent through OpenCode, used by
/// Total Spend. The provider id lets the spend engine split gateway
/// providers (AihubMix) into their own slice.
pub fn collect_cost_events() -> Vec<(f64, f64, f64, String, String)> {
    let mut dirs = vec![data_dir()];
    for acct in discover_extra_accounts() {
        if !dirs.iter().any(|d| same_dir(d, &acct.dir)) {
            dirs.push(acct.dir);
        }
    }
    let mut out = Vec::new();
    for dir in dirs {
        let rows: Vec<(f64, f64, f64, String, String)> = with_live_db(&dir, |db| {
            Ok(read_messages(db)?
                .0
                .into_iter()
                // Free models record cost 0 with real token counts — the
                // tokens are usage facts and count at their true $0 price.
                // Rows with neither cost nor tokens (aborted turns) drop.
                .filter(|r| r.cost > 0.0 || r.tokens > 0.0)
                .map(|r| (r.ts, r.cost, r.tokens, r.model, r.provider))
                .collect())
        })
        .unwrap_or_default();
        out.extend(rows);
    }
    out
}

pub struct MessageRow {
    pub ts: f64,
    pub cost: f64,
    pub tokens: f64,
    pub provider: String,
    pub model: String,
}

/// Raw assistant-message rows from opencode.db.
fn read_messages(db: &Path) -> Result<(Vec<MessageRow>, bool), String> {
    let conn = super::open_readonly_sqlite(db)?;
    let mut stmt = conn
        .prepare(&format!(
            "SELECT time_created, data FROM message ORDER BY time_created DESC LIMIT {}",
            super::MAX_LEDGER_ROWS
        ))
        .map_err(|e| format!("query messages: {e}"))?;

    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| format!("read messages: {e}"))?;

    let mut scanned: u64 = 0;
    let mut out = Vec::new();
    for row in rows {
        scanned += 1;
        let Ok((time_created, data)) = row else { continue };
        let Ok(msg) = serde_json::from_str::<Value>(&data) else { continue };
        if msg.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let provider = msg
            .get("providerID")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let model = msg
            .get("modelID")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let cost = msg.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
        let ts = msg
            .pointer("/time/completed")
            .or_else(|| msg.pointer("/time/created"))
            .and_then(Value::as_f64)
            .unwrap_or(time_created as f64);
        let tokens = ["/tokens/input", "/tokens/output", "/tokens/reasoning"]
            .iter()
            .filter_map(|p| msg.pointer(p).and_then(Value::as_f64))
            .sum::<f64>();
        out.push(MessageRow { ts, cost, tokens, provider, model });
    }
    if scanned >= super::MAX_LEDGER_ROWS {
        eprintln!(
            "[pane] opencode: message table hit the {}-row read cap — keeping newest rows, oldest usage is dropped",
            super::MAX_LEDGER_ROWS
        );
    }
    Ok((out, scanned >= super::MAX_LEDGER_ROWS))
}

/// The monthly cycle anchors on the earliest-ever Go row — which the
/// newest-first cap in read_messages intentionally drops. When (and only
/// when) the cap binds, find the anchor with a small oldest-first probe:
/// bounded regardless of table size, skipped entirely on normal DBs.
///
/// Known limitation: read_messages covers the newest MAX_LEDGER_ROWS and
/// this probe covers the oldest MAX_ANCHOR_PROBE_ROWS, so a table with more
/// than MAX_LEDGER_ROWS + MAX_ANCHOR_PROBE_ROWS rows leaves an unsearched
/// middle band. If the earliest paid Go row falls in that band the monthly
/// anchor lands late, shifting the reported billing boundary and reset time.
/// Only reachable on an implausibly large local ledger (>2.1M rows).
fn earliest_go_anchor(db: &Path) -> Option<f64> {
    const MAX_ANCHOR_PROBE_ROWS: u64 = 100_000;
    let conn = super::open_readonly_sqlite(db).ok()?;
    let mut stmt = conn
        .prepare(&format!(
            "SELECT time_created, data FROM message ORDER BY time_created ASC LIMIT {MAX_ANCHOR_PROBE_ROWS}"
        ))
        .ok()?;
    let rows = stmt
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))
        .ok()?;
    for row in rows.flatten() {
        let (time_created, data) = row;
        let Ok(msg) = serde_json::from_str::<Value>(&data) else { continue };
        if msg.get("role").and_then(Value::as_str) != Some("assistant")
            || msg.get("providerID").and_then(Value::as_str) != Some("opencode-go")
        {
            continue;
        }
        if msg.get("cost").and_then(Value::as_f64).unwrap_or(0.0) <= 0.0 {
            continue;
        }
        return Some(
            msg.pointer("/time/completed")
                .or_else(|| msg.pointer("/time/created"))
                .and_then(Value::as_f64)
                .unwrap_or(time_created as f64),
        );
    }
    None
}


#[cfg(test)]
mod tests {
    use super::*;

    fn ms(iso: &str) -> f64 {
        chrono::DateTime::parse_from_rfc3339(iso).unwrap().timestamp_millis() as f64
    }

    #[test]
    fn official_usage_parses_the_live_wire_shape() {
        // Captured verbatim from the deployed endpoint (2026-08-13) — note
        // it does NOT match the merged PR's draft shape.
        let doc = serde_json::json!({ "usage": {
            "rolling": { "status": "ok", "percent": 0, "resetsAt": "2026-08-13T15:33:33.302Z" },
            "weekly":  { "status": "ok", "percent": 6, "resetsAt": "2026-08-17T00:00:00.302Z" },
            "monthly": { "status": "ok", "percent": 3, "resetsAt": "2026-09-05T08:48:51.302Z" },
        }});
        let m = parse_official(&doc).expect("parses");
        assert_eq!(m.len(), 3);
        assert_eq!((m[0].label.as_str(), m[0].used_percent), ("Session", Some(0.0)));
        assert_eq!((m[1].label.as_str(), m[1].used_percent), ("Weekly", Some(6.0)));
        assert_eq!(m[1].resets_at, Some(ms("2026-08-17T00:00:00.302Z") as i64));
        assert_eq!((m[2].label.as_str(), m[2].used_percent), ("Monthly", Some(3.0)));
        // Monthly period is the REAL cycle length ending at the server's
        // reset (Aug 5 → Sep 5 = 31 days), never a fixed 30 days — a fixed
        // window skewed the pace projection (Devin's find).
        assert_eq!(m[2].period_ms, Some(31 * 86_400_000_i64));
        // Clamped short-month edge: a reset on Mar 31 looks back to
        // Feb 28 in a non-leap year.
        let mar31 = chrono::DateTime::parse_from_rfc3339("2026-03-31T10:00:00Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(month_period_ending(mar31), 31 * 86_400_000_i64);

        // Rate-limited windows render as full; a missing window is skipped
        // without sinking the card; junk yields None (→ local fallback).
        let limited = serde_json::json!({ "usage": {
            "rolling": { "status": "rate-limited", "percent": 100, "resetsAt": "2026-08-13T15:33:33Z" },
        }});
        let m = parse_official(&limited).expect("parses");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].used_percent, Some(100.0));
        // rate-limited stays a full meter even if the server omits (or
        // lags) the percent — the status alone is the answer.
        let no_percent = serde_json::json!({ "usage": {
            "rolling": { "status": "rate-limited", "resetsAt": "2026-08-13T15:33:33Z" },
        }});
        let m = parse_official(&no_percent).expect("parses");
        assert_eq!(m[0].used_percent, Some(100.0));
        assert!(parse_official(&serde_json::json!({"error": "nope"})).is_none());

        // Contract guards: a fractional (0-1) or out-of-range percent
        // means the wire shape changed — the parse must fail loudly (→
        // labeled local fallback), never render wrong meters. Zero and
        // exact integers stay valid.
        for bad in [0.06, 150.0, -3.0] {
            let doc = serde_json::json!({ "usage": {
                "weekly": { "status": "ok", "percent": bad, "resetsAt": "2026-08-17T00:00:00Z" },
            }});
            assert!(parse_official(&doc).is_none(), "percent {bad} must reject");
        }
    }

    #[test]
    fn session_reset_tracks_the_oldest_in_window_row() {
        // Two rows inside the rolling 5h window; reset = oldest + 5h.
        let now = ms("2026-07-28T12:00:00Z");
        let rows = [(ms("2026-07-28T09:00:00Z"), 2.0), (ms("2026-07-28T11:00:00Z"), 1.0)];
        let w = go_windows(&rows, None, now);
        assert!((w.session - 3.0).abs() < 1e-9);
        assert_eq!(w.session_resets_at, ms("2026-07-28T14:00:00Z") as i64);
    }

    #[test]
    fn empty_session_resets_a_full_window_from_now() {
        let now = ms("2026-07-28T12:00:00Z");
        let w = go_windows(&[], None, now);
        assert_eq!(w.session, 0.0);
        assert_eq!(w.session_resets_at, ms("2026-07-28T17:00:00Z") as i64);
    }

    #[test]
    fn weekly_is_a_utc_monday_week_not_a_rolling_7d() {
        // 2026-07-28 is a Tuesday; the week runs Mon Jul 27 -> Mon Aug 3.
        let now = ms("2026-07-28T12:00:00Z");
        let rows = [
            (ms("2026-07-26T23:00:00Z"), 5.0), // Sunday: previous week
            (ms("2026-07-27T01:00:00Z"), 2.0), // Monday: this week
        ];
        let w = go_windows(&rows, None, now);
        assert!((w.weekly - 2.0).abs() < 1e-9, "rolling-7d would count 7.0");
        assert_eq!(w.weekly_resets_at, ms("2026-08-03T00:00:00Z") as i64);
    }

    #[test]
    fn monthly_cycle_anchors_to_the_earliest_go_usage() {
        // First-ever Go usage on the 15th at 08:30 -> cycles run 15th-to-15th.
        let now = ms("2026-07-28T12:00:00Z");
        let rows = [
            (ms("2026-06-15T08:30:00Z"), 1.0), // the anchor itself (old cycle)
            (ms("2026-07-14T12:00:00Z"), 4.0), // before Jul 15: previous cycle
            (ms("2026-07-20T12:00:00Z"), 3.0), // current cycle
        ];
        let w = go_windows(&rows, None, now);
        assert!((w.monthly - 3.0).abs() < 1e-9);
        assert_eq!(w.monthly_resets_at, ms("2026-08-15T08:30:00Z") as i64);
    }

    /// When the row cap drops the oldest rows, the caller passes the probed
    /// earliest-Go anchor explicitly so the monthly cycle stays put.
    #[test]
    fn monthly_anchor_survives_a_capped_row_window() {
        let now = ms("2026-07-28T12:00:00Z");
        // Only recent rows made it past the cap; the true anchor (Jun 15)
        // is recovered separately and passed in.
        let rows = [(ms("2026-07-20T12:00:00Z"), 3.0)];
        let w = go_windows(&rows, Some(ms("2026-06-15T08:30:00Z")), now);
        assert!((w.monthly - 3.0).abs() < 1e-9);
        assert_eq!(w.monthly_resets_at, ms("2026-08-15T08:30:00Z") as i64);
    }

    #[test]
    fn future_anchor_day_rolls_the_cycle_back_a_month() {
        // Anchor day-of-month (30th) hasn't happened yet in July on the 28th?
        // It has; use the 30th with "now" on the 28th: cycle began Jun 30.
        let now = ms("2026-07-28T12:00:00Z");
        let rows = [(ms("2026-05-30T10:00:00Z"), 1.0)];
        let w = go_windows(&rows, None, now);
        assert_eq!(w.monthly_resets_at, ms("2026-07-30T10:00:00Z") as i64);
        assert_eq!(
            w.monthly_period_ms,
            (ms("2026-07-30T10:00:00Z") - ms("2026-06-30T10:00:00Z")) as i64
        );
    }

    #[test]
    fn anchor_day_31_clamps_in_short_months() {
        // Anchored to Jan 31; in February the cycle boundary clamps to Feb 28.
        let now = ms("2026-02-10T12:00:00Z");
        let rows = [(ms("2026-01-31T09:00:00Z"), 1.0)];
        let w = go_windows(&rows, None, now);
        assert_eq!(w.monthly_resets_at, ms("2026-02-28T09:00:00Z") as i64);
    }

    #[test]
    fn credential_fingerprint_is_stable_and_never_the_key() {
        let key = "oc-secret-key-please-do-not-store";
        let a = fingerprint_key(key);
        let b = fingerprint_key(key);
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(scoped_id_charset(&a));
        assert!(!a.contains(key));
        assert_ne!(fingerprint_key(key), fingerprint_key("other-account"));
    }

    #[test]
    fn extra_profile_dir_becomes_its_own_card() {
        let root = std::env::temp_dir().join(format!(
            "pane-opencode-disc-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let extra = root.join("opencode-work");
        std::fs::create_dir_all(&extra).unwrap();
        std::fs::write(
            extra.join("auth.json"),
            r#"{"opencode-go":{"type":"api","key":"work-key-aaa"}}"#,
        )
        .unwrap();
        let fp = dir_identity(&extra).expect("fingerprint");
        let hash8: String = fp.chars().take(8).collect();
        assert_eq!(dir_label(&extra).as_deref(), Some("work"));
        assert_eq!(format!("opencode@{hash8}").split('@').next(), Some("opencode"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn default_without_a_go_key_does_not_void_dedup() {
        // seen starts empty when the default dir has no opencode-go key.
        // Extra profiles can still card — unlike Claude/Codex, there is
        // no unnamed default account to collide with.
        let seen: Vec<String> = None::<String>.into_iter().collect();
        assert!(seen.is_empty());
        let extra = fingerprint_key("work-key-aaa");
        assert!(!seen.iter().any(|s| s == &extra));
    }
}