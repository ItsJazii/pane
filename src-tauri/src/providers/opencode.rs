use super::{Metric, Snapshot};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
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

/// Every `opencode*.db` channel database in `dir`, path-sorted. OpenCode
/// partitions storage by release channel (`opencode.db` for stable,
/// `opencode-next.db` etc. for other channels), so all of them are
/// unioned; the `.db` suffix keeps `-wal`/`-shm` sidecars out. A missing
/// or unlistable dir reads as empty — the "never used OpenCode" case.
fn opencode_dbs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                            n.starts_with("opencode") && n.ends_with(".db")
                        })
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn table_exists(conn: &rusqlite::Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        rusqlite::params![name],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

/// Reads an entry like {"opencode-go": {"type": "api", "key": "..."}} —
/// from OpenCode 2's `credential` table once it is authoritative, else
/// from OpenCode 1's auth.json. Also used by the OpenRouter provider.
pub fn auth_entry_key(entry: &str) -> Option<String> {
    auth_entry_key_in(&data_dir(), entry)
}

fn auth_entry_key_in(dir: &Path, entry: &str) -> Option<String> {
    match resolve_entry_key(dir, entry) {
        CredResult::Key(k) => Some(k),
        _ => None,
    }
}

/// Outcome of resolving one integration's key for a data dir.
enum CredResult {
    /// A key, wherever it lived.
    Key(String),
    /// Stores exist but hold no row for this integration — an OpenCode 2
    /// logout or an auth.json without the entry.
    None,
    /// Nothing to read at all: no auth.json, no channel databases.
    Missing,
    /// A store was present but could not be read — never mistake a
    /// locked database or half-written file for a logout.
    Unreadable,
}

/// OpenCode 2's legacy-credential import marker: once this migration row
/// exists in a channel database, that DB's `credential` table is the
/// authoritative login record and `auth.json` must not revive a logout.
const CRED_IMPORT_MIGRATION: &str = "20260805200742_import_legacy_credentials";

enum DbCred {
    /// The DB never imported file credentials (pre-OpenCode-2 store).
    NotImported,
    /// Imported: `credential` is authoritative — Some(key) or a genuine
    /// absence (logout) that auth.json must not override.
    Found(Option<String>),
}

/// `integration`'s current key in one channel database, or its absence.
/// Row choice mirrors OpenCode's own ordering (active first, then the
/// most recently updated).
fn db_entry_key(db: &Path, integration: &str) -> Result<DbCred, String> {
    let conn = super::open_readonly_sqlite(db)?;
    if !table_exists(&conn, "migration") || !table_exists(&conn, "credential") {
        return Ok(DbCred::NotImported);
    }
    let imported: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM migration WHERE id = ?1",
            rusqlite::params![CRED_IMPORT_MIGRATION],
            |r| r.get(0),
        )
        .map_err(|e| format!("read migrations: {e}"))?;
    if imported == 0 {
        return Ok(DbCred::NotImported);
    }
    use rusqlite::OptionalExtension;
    let key = conn
        .query_row(
            "SELECT json_extract(value, '$.key') FROM credential
             WHERE integration_id = ?1 AND (active IS NULL OR active = 1)
             ORDER BY active DESC, time_updated DESC, id DESC
             LIMIT 1",
            rusqlite::params![integration],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(|e| format!("read credentials: {e}"))?
        .flatten();
    Ok(DbCred::Found(
        key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty()),
    ))
}

/// Resolve `entry`'s key: each channel DB's `credential` table once its
/// import migration ran (first key wins), else auth.json. A key found in
/// any DB returns immediately; a DB read failure beats "imported but
/// absent" (the locked DB might be the one holding the key); and only
/// when NO imported DB exists does the file get a say.
fn resolve_entry_key(dir: &Path, entry: &str) -> CredResult {
    let mut imported = false;
    let mut failed = false;
    for db in opencode_dbs(dir) {
        match db_entry_key(&db, entry) {
            Ok(DbCred::Found(Some(k))) => return CredResult::Key(k),
            Ok(DbCred::Found(None)) => imported = true,
            Ok(DbCred::NotImported) => {}
            Err(_) => failed = true,
        }
    }
    if failed {
        return CredResult::Unreadable;
    }
    if imported {
        return CredResult::None;
    }
    auth_json_key(dir, entry)
}

/// `entry`'s key from the dir's auth.json — the pre-OpenCode-2 store.
fn auth_json_key(dir: &Path, entry: &str) -> CredResult {
    let path = dir.join("auth.json");
    if !path.exists() {
        return CredResult::Missing;
    }
    let Ok(raw) = super::read_small_text(&path, MAX_AUTH_BYTES, "auth.json") else {
        return CredResult::Unreadable;
    };
    let Ok(doc) = serde_json::from_str::<Value>(&raw) else {
        return CredResult::Unreadable;
    };
    doc.get(entry)
        .and_then(|e| e.get("key"))
        .and_then(Value::as_str)
        .filter(|k| !k.is_empty())
        .map(|k| CredResult::Key(k.to_string()))
        .unwrap_or(CredResult::None)
}

/// Channel databases that belong to THIS Go login — the local-window
/// fallback must only meter the account the card shows (channels can
/// hold different Go accounts; spend deliberately stays combined). An
/// imported `credential` table is per-channel truth: `Found(Some)` must
/// equal the key and `Found(None)` is a logout in that channel; a
/// pre-import DB inherits the dir's auth.json key; an unreadable DB is
/// excluded.
fn go_dbs(dir: &Path, key: &str) -> Vec<PathBuf> {
    let file_key = match auth_json_key(dir, "opencode-go") {
        CredResult::Key(k) => Some(k),
        _ => None,
    };
    opencode_dbs(dir)
        .into_iter()
        .filter(|db| match db_entry_key(db, "opencode-go") {
            Ok(DbCred::Found(Some(k))) => k == key,
            Ok(DbCred::Found(None)) => false,
            Ok(DbCred::NotImported) => file_key.as_deref() == Some(key),
            Err(_) => false,
        })
        .collect()
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
    match dir_auth_state(dir) {
        DirAuth::Identified(fp) => Some(fp),
        _ => None,
    }
}

/// Parsed-no-Go is safe to keep discovering (OpenRouter-only default).
/// An existing auth.json that we could not read or parse might be a
/// truncate-and-rewrite; abort extras so the later default snapshot
/// cannot card the same key twice.
enum DirAuth {
    Identified(String),
    ParsedNoGo,
    Unreadable,
    Missing,
}

fn dir_auth_state(dir: &Path) -> DirAuth {
    match resolve_entry_key(dir, "opencode-go") {
        CredResult::Key(k) => DirAuth::Identified(fingerprint_key(&k)),
        CredResult::None => DirAuth::ParsedNoGo,
        CredResult::Missing => DirAuth::Missing,
        CredResult::Unreadable => DirAuth::Unreadable,
    }
}

/// The default login's account identity, for the snapshot-cache stamp.
pub fn default_identity() -> Option<String> {
    dir_identity(&data_dir())
}

pub struct OpenCodeAccount {
    pub id: String,
    pub name: String,
    pub dir: PathBuf,
    pub fingerprint: String,
}

/// Extra OpenCode profiles beyond `~/.local/share/opencode`: OPENCODE_HOME,
/// `~/.local/share/opencode-*`, and the same scan roots Claude/Codex use.
/// A dir that can't name its account never becomes a card; a dir whose
/// fingerprint matches an already-seen login is skipped.
pub fn discover_extra_accounts() -> Vec<OpenCodeAccount> {
    discover_from(&data_dir(), extra_data_dirs(), true)
}

fn discover_from(
    default: &Path,
    extras: Vec<PathBuf>,
    abort_unreadable_default: bool,
) -> Vec<OpenCodeAccount> {
    // Claude/Codex abort when the default login exists but can't be
    // named. OpenCode splits that: a parsed file with no Go key is
    // OpenRouter-only (empty `seen`, extras still card). A file that
    // exists but will not parse is treated as mid-write — usage cards
    // stay hidden so a later successful default snapshot cannot
    // duplicate. Spend still walks extras (`abort_unreadable_default`
    // = false) so those ledgers are not dropped for one rewrite.
    let mut seen = Vec::new();
    match dir_auth_state(default) {
        DirAuth::Unreadable if abort_unreadable_default => return Vec::new(),
        DirAuth::Unreadable | DirAuth::ParsedNoGo | DirAuth::Missing => {}
        DirAuth::Identified(fp) => seen.push(fp),
    }

    let mut out = Vec::new();
    for dir in extras {
        if same_dir(&dir, default) {
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
            fingerprint: fp,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Extra homes that have a Go key, including dirs that share a
/// fingerprint with the default login. Cards stay one-per-fingerprint;
/// spend merges every ledger onto the matching card.
pub fn extra_ledger_homes() -> Vec<(String, String, PathBuf)> {
    extra_ledger_homes_from(&data_dir(), extra_data_dirs())
}

fn extra_ledger_homes_from(
    default: &Path,
    extras: Vec<PathBuf>,
) -> Vec<(String, String, PathBuf)> {
    let default_fp = match dir_auth_state(default) {
        DirAuth::Identified(fp) => Some(fp),
        _ => None,
    };
    let mut seen_dirs: Vec<PathBuf> = Vec::new();
    let mut out = Vec::new();
    for dir in extras {
        if same_dir(&dir, &default) {
            continue;
        }
        if seen_dirs.iter().any(|d| same_dir(d, &dir)) {
            continue;
        }
        let Some(fp) = dir_identity(&dir) else { continue };
        if !scoped_id_charset(&fp) {
            continue;
        }
        seen_dirs.push(dir.clone());
        let (id, name) = if default_fp.as_ref() == Some(&fp) {
            (ID.to_string(), NAME.to_string())
        } else {
            let hash8: String = fp.chars().take(8).collect();
            let name = match dir_label(&dir) {
                Some(l) => format!("OpenCode — {l}"),
                None => format!("OpenCode @{hash8}"),
            };
            (format!("opencode@{hash8}"), name)
        };
        out.push((id, name, dir));
    }
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
    for root in super::account_scan_roots() {
        if !opencode_dbs(&root).is_empty() || root.join("auth.json").is_file() {
            dirs.push(root);
        }
    }
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

pub async fn snapshot() -> Snapshot {
    snapshot_at(data_dir(), ID.to_string(), NAME.to_string(), None).await
}

pub async fn snapshot_at(
    dir: PathBuf,
    id: String,
    name: String,
    expected_fp: Option<String>,
) -> Snapshot {
    match fetch(&dir, &id, &name, expected_fp.as_deref()).await {
        Ok(s) => s,
        Err(e) => Snapshot::error(&id, &name, e),
    }
}

async fn fetch(
    dir: &Path,
    id: &str,
    name: &str,
    expected_fp: Option<&str>,
) -> Result<Snapshot, String> {
    let key = match resolve_entry_key(dir, "opencode-go") {
        CredResult::Key(k) => k,
        // A locked DB or half-written store is not a logout — keep the
        // last-good numbers instead of painting "no credentials".
        CredResult::Unreadable => {
            return Err("couldn't read OpenCode's local credential store".into())
        }
        CredResult::None => {
            return Ok(Snapshot::no_credentials(
                id,
                name,
                "No OpenCode Go subscription found. Log into OpenCode Go.",
            ))
        }
        CredResult::Missing => {
            return Ok(Snapshot::no_credentials(
                id,
                name,
                "OpenCode sign-in not found. Run `opencode` and log in.",
            ))
        }
    };
    if let Some(expected) = expected_fp {
        if fingerprint_key(&key) != expected {
            return Err("OpenCode login changed during refresh.".into());
        }
    }

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
            let mut snap = local_windows_snapshot(dir, id, name, &key)
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
fn local_windows_snapshot(dir: &Path, id: &str, name: &str, key: &str) -> Result<Snapshot, String> {
    // Query the live ledgers read-only — only the channel DBs that hold
    // THIS Go login (`go_dbs`); other channels may belong to a different
    // account. Copying db+WAL into `%APPDATA%\Pane\tmp` used the same
    // pattern that grew Devin's temp journal to tens of GB — never clone
    // a vendor database onto C:.
    let (msgs, capped) = read_messages(dir, key)?;
    let w = {
        let rows: Vec<(f64, f64)> = msgs
            .into_iter()
            .filter(|r| r.provider == "opencode-go" && r.cost > 0.0)
            .map(|r| (r.ts, r.cost))
            .collect();
        // The monthly anchor is the earliest-ever Go row; when the row cap
        // dropped the oldest rows, recover it with the bounded oldest-first
        // probe instead of re-anchoring on the newest window's edge.
        let anchor = if capped { earliest_go_anchor(dir, key) } else { None };
        go_windows(&rows, anchor, chrono::Utc::now().timestamp_millis() as f64)
    };
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
    collect_cost_events_in(&data_dir())
}

pub fn collect_cost_events_in(dir: &Path) -> Vec<(f64, f64, f64, String, String)> {
    let mut rows: Vec<CostRow> = Vec::new();
    for db in opencode_dbs(dir) {
        rows.extend(collect_db_cost_events(&db));
    }
    // OpenCode 2 copies legacy `message` rows into `session_message`
    // under the same id — the union would count each copied row twice.
    // The session_message copy wins even when its channel DB sorts after
    // the v1 copy's, so dedupe in two passes: every v2 row across every
    // DB first, then v1 rows with an unseen id. Rows with no id still
    // count — nothing to dedupe them by.
    let (v2, v1): (Vec<CostRow>, Vec<CostRow>) = rows.into_iter().partition(|r| r.6);
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for (ts, cost, tokens, model, provider, id, _v2) in v2.into_iter().chain(v1) {
        if !id.is_empty() && !seen.insert(id) {
            continue;
        }
        out.push((ts, cost, tokens, model, provider));
    }
    out
}

/// (ts, cost, tokens, model, provider, id, from session_message) — the id
/// and source flag ride along so callers can dedupe OpenCode 2's copied
/// rows across tables and DBs, v2 winning.
type CostRow = (f64, f64, f64, String, String, String, bool);

/// (mtime as epoch ms, length) — a change means new writes landed.
type Stamp = (u64, u64);

fn file_stamp(path: &Path) -> Stamp {
    std::fs::metadata(path)
        .map(|m| {
            let ms = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            (ms, m.len())
        })
        .unwrap_or((0, 0))
}

/// Per-table incremental state for one channel database.
#[derive(Default)]
struct TableCache {
    /// id → (time_created, spend row or None when scanned-but-not-spend —
    /// in-progress, zero-cost, non-assistant). Keeping the id either way
    /// lets the row-count check catch deletions.
    rows: HashMap<String, (i64, Option<CostRow>)>,
    /// High-water mark on `time_updated`: only rows touched at-or-after
    /// it are re-read. Verified on a live OpenCode 2 database — a row's
    /// time_updated bumps to its completion instant, so an assistant row
    /// that gains cost later always resurfaces.
    watermark: i64,
}

/// One channel database's ledger cache: per-table incremental state
/// plus the db/WAL stamps it was last fully-refreshed against.
#[derive(Default)]
struct DbCache {
    db_stamp: Stamp,
    wal_stamp: Stamp,
    v2: TableCache,
    v1: TableCache,
}

impl DbCache {
    fn table_mut(&mut self, v2: bool) -> &mut TableCache {
        if v2 { &mut self.v2 } else { &mut self.v1 }
    }
}

/// A cached DB's spend rows inside the current 31-day window — cached
/// rows can sit for days, so rows that aged out are dropped on serve.
fn cached_spend_rows(c: &DbCache) -> Vec<CostRow> {
    let cutoff_ms = crate::spend::spend_cutoff_ms(chrono::Local::now()) as f64;
    [&c.v2, &c.v1]
        .into_iter()
        .flat_map(|t| t.rows.values())
        .filter(|(tc, _)| {
            let tc = *tc as f64;
            if tc > 1_000_000_000_000.0 { tc >= cutoff_ms } else { tc >= cutoff_ms / 1000.0 }
        })
        .filter_map(|(_, r)| r.clone())
        .collect()
}

/// Rows touched at-or-after `watermark` in one ledger table, upserted
/// into `map` by id. `data` is projected through
/// `json_remove('$.content')` in SQL — v2 assistant rows embed whole
/// tool transcripts (over a gigabyte a month on this machine), and the
/// fields Pane needs never live in `content`. Malformed blobs come back
/// NULL via `json_valid` and are skipped without aborting neighbours.
/// Returns (raw rows scanned, max time_updated seen).
fn scan_incremental(
    conn: &rusqlite::Connection,
    table: &str,
    v2: bool,
    cutoff: i64,
    watermark: i64,
    map: &mut HashMap<String, (i64, Option<CostRow>)>,
) -> Result<(u64, i64), String> {
    let sql = if v2 {
        format!(
            "SELECT id, time_created, time_updated, type,
                    CASE WHEN json_valid(data) THEN json_remove(data, '$.content') END
             FROM {table}
             WHERE time_updated >= {watermark} AND time_created >= {cutoff}
               AND type IN ('assistant','compaction')
             ORDER BY rowid DESC LIMIT {}",
            super::MAX_LEDGER_ROWS
        )
    } else {
        format!(
            "SELECT id, time_created, time_updated,
                    CASE WHEN json_valid(data) THEN json_remove(data, '$.content') END
             FROM {table}
             WHERE time_updated >= {watermark} AND time_created >= {cutoff}
             ORDER BY rowid DESC LIMIT {}",
            super::MAX_LEDGER_ROWS
        )
    };
    let data_col = if v2 { 4 } else { 3 };
    let mut stmt = conn.prepare(&sql).map_err(|e| format!("query ledger: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0).unwrap_or_default(),
                row.get::<_, i64>(1).unwrap_or(0),
                row.get::<_, i64>(2).unwrap_or(0),
                row.get::<_, String>(3).unwrap_or_default(),
                row.get::<_, Option<String>>(data_col).unwrap_or(None),
            ))
        })
        .map_err(|e| format!("read ledger: {e}"))?;
    let mut scanned = 0u64;
    let mut max_tu = watermark;
    for row in rows {
        scanned += 1;
        let Ok((id, tc, tu, ty, data)) = row else { continue };
        max_tu = max_tu.max(tu);
        let shape = if v2 {
            match ty.as_str() {
                "assistant" => RowShape::V2Assistant,
                "compaction" => RowShape::V2Compaction,
                // The SQL type filter already ran; anything else simply
                // is not spend.
                _ => continue,
            }
        } else {
            RowShape::V1
        };
        if id.is_empty() {
            continue;
        }
        let spend = data
            .as_deref()
            .and_then(|d| ledger_row(shape, tc, d))
            .and_then(|m| {
                (m.cost > 0.0 || m.tokens > 0.0)
                    .then_some((m.ts, m.cost, m.tokens, m.model, m.provider, id.clone(), v2))
            });
        map.insert(id, (tc, spend));
    }
    Ok((scanned, max_tu))
}

/// One channel database's spend rows, served from a per-table
/// incremental cache. A stamp hit serves cached rows window-filtered;
/// otherwise each present table is refreshed by `time_updated` watermark
/// (first read scans everything in-window), with a row-count check that
/// catches deletions and backdated inserts — a mismatch rescans the
/// table from scratch, as does a shrunk db file. A failed table keeps
/// its previous map and watermark and the DB stamps stay old, so the
/// next refresh retries it; every-present-table-failed with an empty
/// cache is just an empty result, retried next refresh.
static SPEND_CACHE: std::sync::LazyLock<
    std::sync::Mutex<HashMap<PathBuf, DbCache>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

fn collect_db_cost_events(db_path: &Path) -> Vec<CostRow> {
    if !db_path.exists() {
        return Vec::new();
    }
    let db_stamp = file_stamp(db_path);
    let wal_stamp = file_stamp(&db_path.with_extension("db-wal"));

    if let Ok(cache) = SPEND_CACHE.lock() {
        if let Some(c) = cache.get(db_path) {
            if c.db_stamp == db_stamp && c.wal_stamp == wal_stamp {
                return cached_spend_rows(c);
            }
        }
    }

    let Ok(conn) = super::open_readonly_sqlite(db_path) else {
        return SPEND_CACHE
            .lock()
            .ok()
            .and_then(|c| c.get(db_path).map(cached_spend_rows))
            .unwrap_or_default();
    };

    let cutoff_ms = crate::spend::spend_cutoff_ms(chrono::Local::now());
    let mut cache = SPEND_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    // A shrunk db means the file was replaced or vacuumed — cached ids
    // may not exist anymore; rescan from scratch. WAL length is not a
    // signal: checkpoints shrink it legitimately.
    if cache
        .get(db_path)
        .is_some_and(|c| db_stamp.1 < c.db_stamp.1)
    {
        cache.remove(db_path);
    }
    let entry = cache
        .entry(db_path.to_path_buf())
        .or_insert_with(|| DbCache { db_stamp, wal_stamp, ..Default::default() });

    let mut complete = true;
    for (table, v2) in [("session_message", true), ("message", false)] {
        if !table_exists(&conn, table) {
            continue;
        }
        let cutoff = table_cutoff(&conn, table, cutoff_ms);
        let tcache = entry.table_mut(v2);
        let (mut scanned, mut max_tu) =
            match scan_incremental(&conn, table, v2, cutoff, tcache.watermark, &mut tcache.rows)
            {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[pane] opencode: {table} read failed ({e})");
                    complete = false;
                    continue;
                }
            };
        // New inserts already arrived via the watermark above, so the
        // in-window row count must now match the map exactly; a
        // shortfall means deletions or a rewrite the watermark can't
        // see — rescan the table from scratch. A failed count (-1)
        // leaves the map alone.
        let type_filter = if v2 { " AND type IN ('assistant','compaction')" } else { "" };
        let db_count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE time_created >= {cutoff}{type_filter}"),
                [],
                |r| r.get(0),
            )
            .unwrap_or(-1);
        let cached_count = tcache
            .rows
            .values()
            .filter(|(tc, _)| *tc >= cutoff)
            .count() as i64;
        if db_count >= 0 && db_count != cached_count {
            tcache.rows.clear();
            match scan_incremental(&conn, table, v2, cutoff, 0, &mut tcache.rows) {
                Ok((s, m)) => {
                    scanned = s;
                    max_tu = m;
                }
                Err(e) => {
                    eprintln!("[pane] opencode: {table} rescan failed ({e})");
                    complete = false;
                    continue;
                }
            }
        }
        if scanned >= super::MAX_LEDGER_ROWS {
            eprintln!(
                "[pane] opencode: {table} table hit the {}-row read cap — keeping newest rows, oldest usage is dropped",
                super::MAX_LEDGER_ROWS
            );
        }
        tcache.watermark = max_tu;
    }
    // Stamps only advance on a complete refresh — a table that failed
    // keeps its old map AND leaves the entry unstamped so the next
    // refresh retries it.
    if complete {
        entry.db_stamp = db_stamp;
        entry.wal_stamp = wal_stamp;
    }
    cached_spend_rows(entry)
}

/// Which ledger shape a row came from. v1 `message` rows carry flat
/// `role`/`providerID`/`modelID`; v2 `session_message` rows put the role
/// in the `type` column (pre-filtered by SQL) and nest model/provider
/// under `model`.
#[derive(Clone, Copy)]
enum RowShape {
    V1,
    V2Assistant,
    V2Compaction,
}

/// One assistant/compaction row parsed into a MessageRow, or None when
/// the blob is junk, a non-assistant v1 row, or an unfinished
/// compaction. Shared by the spend reader, the Go-window reader, and the
/// monthly anchor probe.
fn ledger_row(shape: RowShape, time_created: i64, data: &str) -> Option<MessageRow> {
    let msg = serde_json::from_str::<Value>(data).ok()?;
    match shape {
        RowShape::V1 => {
            if msg.get("role").and_then(Value::as_str) != Some("assistant") {
                return None;
            }
        }
        RowShape::V2Assistant => {}
        RowShape::V2Compaction => {
            // Only a finished compaction carries usage worth counting.
            if msg.get("status").and_then(Value::as_str) != Some("completed") {
                return None;
            }
        }
    }
    let provider = msg
        .pointer("/model/providerID")
        .or_else(|| msg.get("providerID"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let model = msg
        .pointer("/model/id")
        .or_else(|| msg.get("modelID"))
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
    if matches!(shape, RowShape::V2Compaction) && cost <= 0.0 && tokens <= 0.0 {
        return None;
    }
    Some(MessageRow { ts, cost, tokens, provider, model })
}

/// Scan one ledger table into `sink` as (id, parsed row) pairs. `sql`
/// must select id, time_created, (v2 only:) type, then data last.
/// Returns the raw row count so callers can flag a hit row cap.
fn scan_ledger_rows(
    conn: &rusqlite::Connection,
    sql: &str,
    v2: bool,
    sink: &mut Vec<(String, MessageRow)>,
) -> Result<u64, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| format!("query ledger: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            let shape = if v2 {
                match row.get::<_, String>(2).unwrap_or_default().as_str() {
                    "assistant" => RowShape::V2Assistant,
                    "compaction" => RowShape::V2Compaction,
                    _ => return Ok(None),
                }
            } else {
                RowShape::V1
            };
            Ok(Some((
                row.get::<_, String>(0).unwrap_or_default(),
                row.get::<_, i64>(1).unwrap_or(0),
                row.get::<_, String>(if v2 { 3 } else { 2 }).unwrap_or_default(),
                shape,
            )))
        })
        .map_err(|e| format!("read ledger: {e}"))?;
    let mut scanned = 0u64;
    for row in rows {
        scanned += 1;
        let Ok(Some((id, time_created, data, shape))) = row else { continue };
        if let Some(m) = ledger_row(shape, time_created, &data) {
            sink.push((id, m));
        }
    }
    Ok(scanned)
}

/// v1 wrote seconds for a stretch before switching to ms; v2 always
/// writes ms. Pick the cutoff unit from the table's own newest row —
/// json_extract in WHERE made SQLite parse every blob, so the integer
/// clock stays the pre-filter and role/cost stay in Rust.
fn table_cutoff(conn: &rusqlite::Connection, table: &str, cutoff_ms: i64) -> i64 {
    let max_ts: i64 = conn
        .query_row(
            &format!("SELECT COALESCE(MAX(time_created), 0) FROM {table}"),
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if max_ts > 1_000_000_000_000 { cutoff_ms } else { cutoff_ms / 1000 }
}

pub struct MessageRow {
    pub ts: f64,
    pub cost: f64,
    pub tokens: f64,
    pub provider: String,
    pub model: String,
}

/// Assistant/compaction rows across the channel DBs that hold THIS Go
/// login (`go_dbs`), deduped by message id with v2 copies winning across
/// files. `capped` flags any table that hit the row cap so the monthly
/// anchor can be recovered by the oldest-first probe.
fn read_messages(dir: &Path, key: &str) -> Result<(Vec<MessageRow>, bool), String> {
    let dbs = go_dbs(dir, key);
    if dbs.is_empty() {
        return Err("no opencode*.db for this Go login — has OpenCode been used on this PC?".into());
    }
    let mut rows: Vec<(String, MessageRow, bool)> = Vec::new();
    let mut capped = false;
    let mut read_any = false;
    let mut first_err = None;
    for db in &dbs {
        match read_db_messages(db) {
            Ok((r, c)) => {
                read_any = true;
                capped |= c;
                rows.extend(r);
            }
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    if !read_any {
        return Err(first_err.unwrap_or_else(|| "no readable opencode*.db".into()));
    }
    // v2 wins the id dedupe even when its DB sorted after the v1 copy's.
    let (v2, v1): (Vec<_>, Vec<_>) = rows.into_iter().partition(|(_, _, v)| *v);
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for (id, m, _) in v2.into_iter().chain(v1) {
        if !id.is_empty() && !seen.insert(id) {
            continue;
        }
        out.push(m);
    }
    Ok((out, capped))
}

/// One database's newest rows from both tables, tagged with the source
/// (true = session_message) so the caller's dedupe can prefer v2 copies
/// across files. Err only when no present table could be read — a
/// session_message failure never sinks the OpenCode 1 table, and vice
/// versa.
fn read_db_messages(db: &Path) -> Result<(Vec<(String, MessageRow, bool)>, bool), String> {
    let conn = super::open_readonly_sqlite(db)?;
    let mut out = Vec::new();
    let mut capped = false;
    let mut present = 0u32;
    let mut ok = 0u32;
    let mut last_err = None;
    for (table, v2) in [("session_message", true), ("message", false)] {
        if !table_exists(&conn, table) {
            continue;
        }
        present += 1;
        let sql = if v2 {
            format!(
                "SELECT id, time_created, type,
                        CASE WHEN json_valid(data) THEN json_remove(data, '$.content') END
                 FROM {table}
                 WHERE type IN ('assistant','compaction')
                 ORDER BY time_created DESC LIMIT {}",
                super::MAX_LEDGER_ROWS
            )
        } else {
            format!(
                "SELECT id, time_created,
                        CASE WHEN json_valid(data) THEN json_remove(data, '$.content') END
                 FROM {table}
                 ORDER BY time_created DESC LIMIT {}",
                super::MAX_LEDGER_ROWS
            )
        };
        let mut parsed = Vec::new();
        match scan_ledger_rows(&conn, &sql, v2, &mut parsed) {
            Ok(scanned) => {
                ok += 1;
                if scanned >= super::MAX_LEDGER_ROWS {
                    eprintln!(
                        "[pane] opencode: {table} table hit the {}-row read cap — keeping newest rows, oldest usage is dropped",
                        super::MAX_LEDGER_ROWS
                    );
                    capped = true;
                }
                out.extend(parsed.into_iter().map(|(id, m)| (id, m, v2)));
            }
            Err(e) => {
                eprintln!("[pane] opencode: {table} read failed ({e})");
                last_err = Some(e);
            }
        }
    }
    if present > 0 && ok == 0 {
        return Err(last_err.unwrap_or_else(|| "ledger unreadable".into()));
    }
    Ok((out, capped))
}

/// The monthly cycle anchors on the earliest-ever Go row — which the
/// newest-first cap in read_messages intentionally drops. When (and only
/// when) the cap binds, find the anchor with a small oldest-first probe
/// across both tables of every channel DB that holds this login:
/// bounded regardless of table size, skipped entirely on normal DBs.
///
/// Known limitation: read_messages covers the newest MAX_LEDGER_ROWS per
/// table and this probe covers the oldest MAX_ANCHOR_PROBE_ROWS per
/// table, so a table with more than MAX_LEDGER_ROWS +
/// MAX_ANCHOR_PROBE_ROWS rows leaves an unsearched middle band. If the
/// earliest paid Go row falls in that band the monthly anchor lands
/// late, shifting the reported billing boundary and reset time. Only
/// reachable on an implausibly large local ledger (>2.1M rows).
fn earliest_go_anchor(dir: &Path, key: &str) -> Option<f64> {
    const MAX_ANCHOR_PROBE_ROWS: u64 = 100_000;
    let mut best: Option<f64> = None;
    for db in go_dbs(dir, key) {
        let Ok(conn) = super::open_readonly_sqlite(&db) else { continue };
        for (table, v2) in [("session_message", true), ("message", false)] {
            if !table_exists(&conn, table) {
                continue;
            }
            let sql = if v2 {
                format!(
                    "SELECT id, time_created, type,
                            CASE WHEN json_valid(data) THEN json_remove(data, '$.content') END
                     FROM {table}
                     WHERE type IN ('assistant','compaction')
                     ORDER BY time_created ASC LIMIT {MAX_ANCHOR_PROBE_ROWS}"
                )
            } else {
                format!(
                    "SELECT id, time_created,
                            CASE WHEN json_valid(data) THEN json_remove(data, '$.content') END
                     FROM {table}
                     ORDER BY time_created ASC LIMIT {MAX_ANCHOR_PROBE_ROWS}"
                )
            };
            let mut parsed = Vec::new();
            if scan_ledger_rows(&conn, &sql, v2, &mut parsed).is_err() {
                continue;
            }
            // Ascending: the first paid Go row is this table's earliest.
            if let Some((_, m)) =
                parsed.into_iter().find(|(_, m)| m.provider == "opencode-go" && m.cost > 0.0)
            {
                best = Some(best.map_or(m.ts, |b: f64| b.min(m.ts)));
            }
        }
    }
    best
}


#[cfg(test)]
mod tests {
    use super::*;

    fn ms(iso: &str) -> f64 {
        chrono::DateTime::parse_from_rfc3339(iso).unwrap().timestamp_millis() as f64
    }

    #[test]
    fn stamp_cache_drops_rows_past_the_cutoff() {
        let now = chrono::Utc::now().timestamp_millis();
        let old = now - 40 * 86_400_000;
        let fresh = now - 2 * 86_400_000;
        let mut c = DbCache::default();
        c.v2.rows.insert(
            "a".into(),
            (old, Some((old as f64, 1.0, 10.0, "m".into(), "p".into(), "a".into(), true))),
        );
        c.v2.rows.insert(
            "b".into(),
            (fresh, Some((fresh as f64, 2.0, 20.0, "m".into(), "p".into(), "b".into(), true))),
        );
        let kept = cached_spend_rows(&c);
        assert_eq!(kept.len(), 1);
        assert!((kept[0].1 - 2.0).abs() < 1e-9);
    }

    #[test]
    fn malformed_message_blob_does_not_drop_neighbors() {
        let good = r#"{"role":"assistant","providerID":"opencode-go","modelID":"k3","cost":1.5,"tokens":{"input":10,"output":5}}"#;
        let bad = "{not-json";
        let rows: Vec<_> = [good, bad, good]
            .into_iter()
            .filter_map(|data| ledger_row(RowShape::V1, 1_784_208_630, data))
            .collect();
        assert_eq!(rows.len(), 2);
        assert!((rows[0].cost - 1.5).abs() < 1e-9);
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

    fn disc_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "pane-opencode-disc-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn write_auth(dir: &Path, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("auth.json"), body).unwrap();
    }

    #[test]
    fn extra_profile_dir_becomes_its_own_card() {
        let root = disc_root();
        let extra = root.join("opencode-work");
        write_auth(
            &extra,
            r#"{"opencode-go":{"type":"api","key":"work-key-aaa"}}"#,
        );
        let found = discover_from(&root.join("default"), vec![extra.clone()], true);
        let fp = dir_identity(&extra).expect("fingerprint");
        let hash8: String = fp.chars().take(8).collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, format!("opencode@{hash8}"));
        assert_eq!(dir_label(&extra).as_deref(), Some("work"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn openrouter_only_default_still_discovers_extras() {
        let root = disc_root();
        let default = root.join("default");
        let extra = root.join("opencode-work");
        write_auth(
            &default,
            r#"{"openrouter":{"type":"api","key":"or-only"}}"#,
        );
        write_auth(
            &extra,
            r#"{"opencode-go":{"type":"api","key":"work-key-aaa"}}"#,
        );
        let found = discover_from(&default, vec![extra], true);
        assert_eq!(found.len(), 1);
        assert!(found[0].id.starts_with("opencode@"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unreadable_default_auth_hides_extras() {
        let root = disc_root();
        let default = root.join("default");
        let extra = root.join("opencode-work");
        write_auth(&default, "{not-json");
        write_auth(
            &extra,
            r#"{"opencode-go":{"type":"api","key":"work-key-aaa"}}"#,
        );
        assert!(discover_from(&default, vec![extra.clone()], true).is_empty());
        assert_eq!(discover_from(&default, vec![extra], false).len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn extra_ledger_homes_merge_shared_and_default_keys() {
        let root = disc_root();
        let default = root.join("default");
        let work = root.join("opencode-work");
        let copy = root.join("opencode-copy");
        let other = root.join("opencode-other");
        write_auth(
            &default,
            r#"{"opencode-go":{"type":"api","key":"alice-key"}}"#,
        );
        write_auth(
            &work,
            r#"{"opencode-go":{"type":"api","key":"alice-key"}}"#,
        );
        write_auth(
            &copy,
            r#"{"opencode-go":{"type":"api","key":"alice-key"}}"#,
        );
        write_auth(
            &other,
            r#"{"opencode-go":{"type":"api","key":"bob-key"}}"#,
        );
        let found = extra_ledger_homes_from(
            &default,
            vec![work, copy.clone(), copy, other],
        );
        let alice: Vec<_> = found.iter().filter(|(id, _, _)| id == "opencode").collect();
        let bob: Vec<_> = found
            .iter()
            .filter(|(id, _, _)| id.starts_with("opencode@"))
            .collect();
        assert_eq!(alice.len(), 2, "two extra homes share the default key");
        assert_eq!(bob.len(), 1, "one distinct extra login");
        std::fs::remove_dir_all(&root).ok();
    }

    // ---- OpenCode 2: session_message ledger + credential table --------

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pane-oc-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const LEDGER_SCHEMA: &str = "
        CREATE TABLE message (id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
        CREATE TABLE session_message (id TEXT, session_id TEXT, type TEXT, seq INTEGER, time_created INTEGER, time_updated INTEGER, data TEXT);
    ";

    const CRED_SCHEMA: &str = "
        CREATE TABLE credential (id TEXT, integration_id TEXT, label TEXT, value TEXT, connector_id TEXT, method_id TEXT, active INTEGER, time_created INTEGER, time_updated INTEGER);
        CREATE TABLE migration (id TEXT, time_created INTEGER);
    ";

    fn make_db(path: &Path, schema: &str) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(schema).unwrap();
        conn
    }

    fn add_message(conn: &rusqlite::Connection, id: &str, ts: i64, data: &str) {
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1,'s',?2,?2,?3)",
            rusqlite::params![id, ts, data],
        )
        .unwrap();
    }

    fn add_session_message(conn: &rusqlite::Connection, id: &str, ty: &str, ts: i64, data: &str) {
        conn.execute(
            "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES (?1,'s',?2,0,?3,?3,?4)",
            rusqlite::params![id, ty, ts, data],
        )
        .unwrap();
    }

    fn add_credential(
        conn: &rusqlite::Connection,
        integration: &str,
        value: &str,
        active: i64,
        updated: i64,
    ) {
        conn.execute(
            "INSERT INTO credential (id, integration_id, value, active, time_created, time_updated)
             VALUES (lower(hex(randomblob(8))), ?1, ?2, ?3, ?4, ?4)",
            rusqlite::params![integration, value, active, updated],
        )
        .unwrap();
    }

    /// A `credential`/`migration` schema; `migration=true` inserts the
    /// OpenCode 2 legacy-import row that makes the table authoritative.
    fn cred_db(path: &Path, imported: bool) -> rusqlite::Connection {
        let conn = make_db(path, CRED_SCHEMA);
        if imported {
            conn.execute(
                "INSERT INTO migration (id, time_created) VALUES (?1, 0)",
                rusqlite::params![CRED_IMPORT_MIGRATION],
            )
            .unwrap();
        }
        conn
    }

    /// Ledger + credential schema with the import migration applied —
    /// an OpenCode 2 channel DB. `go_key` installs that integration's
    /// credential row (None = logged out in this channel).
    fn full_db(path: &Path, go_key: Option<&str>) -> rusqlite::Connection {
        let conn = make_db(path, LEDGER_SCHEMA);
        conn.execute_batch(CRED_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO migration (id, time_created) VALUES (?1, 0)",
            rusqlite::params![CRED_IMPORT_MIGRATION],
        )
        .unwrap();
        if let Some(k) = go_key {
            let v = serde_json::json!({"type": "api", "key": k}).to_string();
            add_credential(&conn, "opencode-go", &v, 1, 1000);
        }
        conn
    }

    fn v1_blob(provider: &str, model: &str, cost: f64, ts: i64) -> String {
        serde_json::json!({
            "role": "assistant", "providerID": provider, "modelID": model,
            "cost": cost, "time": {"created": ts, "completed": ts},
            "tokens": {"input": 10, "output": 5}
        })
        .to_string()
    }

    fn v2_blob(provider: &str, model: &str, cost: f64, ts: i64) -> String {
        serde_json::json!({
            "agent": "build",
            "model": {"id": model, "providerID": provider, "variant": "xhigh"},
            "cost": cost, "time": {"created": ts, "completed": ts},
            "tokens": {"input": 10, "output": 5, "reasoning": 0}
        })
        .to_string()
    }

    #[test]
    fn v2_assistant_row_parses_the_nested_shape() {
        let data = serde_json::json!({
            "time": {"created": 1792158187712i64, "streamed": 1, "completed": 1792158269508i64},
            "agent": "build",
            "model": {"id": "qwen3.8-flash", "providerID": "opencode-go", "variant": "xhigh"},
            "finish": "tool-calls", "rawFinish": "tool_use",
            "cost": 0.006022268,
            "tokens": {"input": 6, "output": 5172, "reasoning": 7,
                       "cache": {"read": 211358, "write": 1044}}
        })
        .to_string();
        let m = ledger_row(RowShape::V2Assistant, 1792158187712, &data).expect("v2 row parses");
        assert_eq!(m.provider, "opencode-go");
        assert_eq!(m.model, "qwen3.8-flash");
        assert!((m.cost - 0.006022268).abs() < 1e-9);
        assert_eq!(m.ts, 1792158269508.0, "time.completed wins over created");
        // tokens = input+output+reasoning; cache read/write never counted.
        assert_eq!(m.tokens, 6.0 + 5172.0 + 7.0);
    }

    #[test]
    fn v2_in_progress_and_compaction_rows_follow_the_rules() {
        // In-progress assistant: no cost/tokens — parses, but the spend
        // filter (cost>0 || tokens>0) drops it downstream.
        let live = r#"{"model":{"id":"m","providerID":"opencode-go"},"tokens":{"input":0}}"#;
        let m = ledger_row(RowShape::V2Assistant, 1, live).unwrap();
        assert!(m.cost <= 0.0 && m.tokens <= 0.0);
        // A completed compaction with real numbers counts.
        let done = r#"{"status":"completed","model":{"providerID":"opencode-go","id":"c"},"cost":0.5,"tokens":{"input":1,"output":2}}"#;
        let m = ledger_row(RowShape::V2Compaction, 1, done).expect("paid compaction counts");
        assert!((m.cost - 0.5).abs() < 1e-9);
        // …but a compaction without numbers or still running never does.
        let empty = r#"{"status":"completed","model":{"providerID":"opencode-go","id":"c"}}"#;
        assert!(ledger_row(RowShape::V2Compaction, 1, empty).is_none());
        let running = r#"{"status":"running","cost":0.5,"tokens":{"input":1}}"#;
        assert!(ledger_row(RowShape::V2Compaction, 1, running).is_none());
    }

    #[test]
    fn copied_rows_across_both_tables_count_once() {
        let dir = tmp_dir("union");
        {
            let conn = make_db(&dir.join("opencode.db"), LEDGER_SCHEMA);
            let now = chrono::Utc::now().timestamp_millis();
            // Same id in both tables — the session_message copy wins.
            add_message(&conn, "shared", now - 1000, &v1_blob("opencode-go", "k3", 1.0, now - 1000));
            add_session_message(&conn, "shared", "assistant", now - 1000, &v2_blob("opencode-go", "k3", 2.0, now - 1000));
            add_message(&conn, "v1-only", now - 2000, &v1_blob("opencode-go", "k3", 3.0, now - 2000));
            add_session_message(&conn, "v2-only", "assistant", now - 3000, &v2_blob("opencode-go", "k3", 4.0, now - 3000));
            // A non-assistant v2 row never counts.
            add_session_message(&conn, "user-msg", "user", now - 4000, "{}");
        }
        let rows = collect_cost_events_in(&dir);
        assert_eq!(rows.len(), 3);
        let total: f64 = rows.iter().map(|r| r.1).sum();
        assert!(
            (total - (2.0 + 3.0 + 4.0)).abs() < 1e-9,
            "v2 copy (2.0) wins over the 1.0 original; total {total}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opencode1_db_without_session_message_still_reads() {
        let dir = tmp_dir("v1only");
        {
            let conn = rusqlite::Connection::open(dir.join("opencode.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE message (id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);",
            )
            .unwrap();
            let now = chrono::Utc::now().timestamp_millis();
            add_message(&conn, "a", now, &v1_blob("opencode-go", "k3", 1.25, now));
            add_message(&conn, "u", now, r#"{"role":"user"}"#);
        }
        let rows = collect_cost_events_in(&dir);
        assert_eq!(rows.len(), 1);
        assert!((rows[0].1 - 1.25).abs() < 1e-9);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn overlapping_ids_across_db_files_count_once() {
        let dir = tmp_dir("twodb");
        let now = chrono::Utc::now().timestamp_millis();
        {
            let conn = make_db(&dir.join("opencode.db"), LEDGER_SCHEMA);
            add_message(&conn, "shared", now, &v1_blob("opencode-go", "k3", 1.0, now));
            add_message(&conn, "a-only", now, &v1_blob("opencode-go", "k3", 2.0, now));
        }
        {
            // opencode-next.db sorts before opencode.db — its copy wins.
            let conn = make_db(&dir.join("opencode-next.db"), LEDGER_SCHEMA);
            add_session_message(&conn, "shared", "assistant", now, &v2_blob("opencode-go", "k3", 5.0, now));
        }
        let rows = collect_cost_events_in(&dir);
        assert_eq!(rows.len(), 2);
        let total: f64 = rows.iter().map(|r| r.1).sum();
        assert!((total - (5.0 + 2.0)).abs() < 1e-9, "shared counted once; total {total}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_messages_dedupes_both_tables() {
        let dir = tmp_dir("winunion");
        {
            let conn = full_db(&dir.join("opencode.db"), Some("test-key-a"));
            let now = chrono::Utc::now().timestamp_millis();
            add_message(&conn, "x", now, &v1_blob("opencode-go", "k3", 1.0, now));
            add_session_message(&conn, "x", "assistant", now, &v2_blob("opencode-go", "k3", 1.0, now));
            add_session_message(&conn, "y", "assistant", now, &v2_blob("other", "m", 0.5, now));
        }
        let (msgs, capped) = read_messages(&dir, "test-key-a").unwrap();
        assert!(!capped);
        assert_eq!(msgs.len(), 2, "the copied id counts once");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cross_db_v2_copy_beats_the_v1_original() {
        // opencode-next.db sorts FIRST: its legacy `message` copy must not
        // suppress opencode.db's `session_message` copy of the same id.
        let dir = tmp_dir("v2wins");
        let now = chrono::Utc::now().timestamp_millis();
        {
            let conn = make_db(&dir.join("opencode-next.db"), LEDGER_SCHEMA);
            add_message(&conn, "shared", now, &v1_blob("opencode-go", "k3", 1.0, now));
        }
        {
            let conn = make_db(&dir.join("opencode.db"), LEDGER_SCHEMA);
            add_session_message(&conn, "shared", "assistant", now, &v2_blob("opencode-go", "k3", 2.0, now));
        }
        let rows = collect_cost_events_in(&dir);
        assert_eq!(rows.len(), 1);
        assert!((rows[0].1 - 2.0).abs() < 1e-9, "the v2 copy wins; got {}", rows[0].1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_messages_v2_wins_across_dbs_too() {
        let dir = tmp_dir("winv2wins");
        let now = chrono::Utc::now().timestamp_millis();
        {
            let conn = full_db(&dir.join("opencode-next.db"), Some("test-key-a"));
            add_message(&conn, "shared", now, &v1_blob("opencode-go", "k3", 1.0, now));
        }
        {
            let conn = full_db(&dir.join("opencode.db"), Some("test-key-a"));
            add_session_message(&conn, "shared", "assistant", now, &v2_blob("opencode-go", "k3", 2.0, now));
        }
        let (msgs, _) = read_messages(&dir, "test-key-a").unwrap();
        assert_eq!(msgs.len(), 1);
        assert!((msgs[0].cost - 2.0).abs() < 1e-9, "the v2 copy wins; got {}", msgs[0].cost);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn v2_only_db_with_a_broken_table_is_err_not_empty() {
        let dir = tmp_dir("v2broken");
        let db = dir.join("opencode.db");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            // session_message exists but lacks the columns the SELECT
            // needs — the only present table fails, so the read is an
            // Err, not a silent zero.
            conn.execute_batch("CREATE TABLE session_message (id TEXT);").unwrap();
        }
        assert!(collect_db_cost_events(&db).is_empty());
        // Fix the table — the next read returns rows (nothing cached a
        // fake empty result).
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "DROP TABLE session_message;
                 CREATE TABLE session_message (id TEXT, session_id TEXT, type TEXT, seq INTEGER, time_created INTEGER, time_updated INTEGER, data TEXT);",
            )
            .unwrap();
            let now = chrono::Utc::now().timestamp_millis();
            add_session_message(&conn, "v2-only", "assistant", now, &v2_blob("opencode-go", "k3", 4.0, now));
        }
        let rows = collect_cost_events_in(&dir);
        assert_eq!(rows.len(), 1);
        assert!((rows[0].1 - 4.0).abs() < 1e-9);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn partial_read_serves_last_good_and_never_replaces_it() {
        let dir = tmp_dir("partial");
        let db = dir.join("opencode.db");
        let now = chrono::Utc::now().timestamp_millis();
        {
            let conn = make_db(&db, LEDGER_SCHEMA);
            add_session_message(&conn, "v2row", "assistant", now, &v2_blob("opencode-go", "k3", 2.0, now));
            add_message(&conn, "v1row", now, &v1_blob("opencode-go", "k3", 1.0, now));
        }
        // Complete read seeds last-good.
        assert_eq!(collect_cost_events_in(&dir).len(), 2);
        // Break session_message → partial read: last-good (both rows)
        // wins over the live partial {v1row}.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch("DROP TABLE session_message; CREATE TABLE session_message (id TEXT);").unwrap();
        }
        assert_eq!(
            collect_cost_events_in(&dir).len(),
            2,
            "an incomplete read serves last-good, not the partial"
        );
        // Break message too → every present table fails. Still last-good
        // {2 rows}: had the partial been cached, this would return 1.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch("DROP TABLE message; CREATE TABLE message (id TEXT);").unwrap();
        }
        assert_eq!(
            collect_cost_events_in(&dir).len(),
            2,
            "the partial read must never become last-good"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn partial_read_surfaces_good_rows_then_serves_them_on_failure() {
        let dir = tmp_dir("partial-nocache");
        let db = dir.join("opencode.db");
        let now = chrono::Utc::now().timestamp_millis();
        {
            let conn = make_db(&db, LEDGER_SCHEMA);
            add_session_message(&conn, "v2row", "assistant", now, &v2_blob("opencode-go", "k3", 2.0, now));
            // message present but broken → partial read, no last-good yet.
            conn.execute_batch("DROP TABLE message; CREATE TABLE message (id TEXT);").unwrap();
        }
        let rows = collect_cost_events_in(&dir);
        assert_eq!(rows.len(), 1, "the surviving table's rows still surface");
        assert!((rows[0].1 - 2.0).abs() < 1e-9);
        // Now break session_message too: every table fails — but the
        // failed reads leave the maps untouched, so the already-scanned
        // v2 row is still served (and the entry stays unstamped, so the
        // next refresh retries both tables).
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch("DROP TABLE session_message; CREATE TABLE session_message (id TEXT);").unwrap();
        }
        let rows = collect_cost_events_in(&dir);
        assert_eq!(rows.len(), 1, "the cached map outlives a failed read");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn window_read_keeps_v2_rows_when_v1_fails() {
        let dir = tmp_dir("winpartial");
        let now = chrono::Utc::now().timestamp_millis();
        {
            let conn = full_db(&dir.join("opencode.db"), Some("test-key-a"));
            add_session_message(&conn, "v2row", "assistant", now, &v2_blob("opencode-go", "k3", 2.0, now));
            conn.execute_batch("DROP TABLE message; CREATE TABLE message (id TEXT);").unwrap();
        }
        let (msgs, _) = read_messages(&dir, "test-key-a").unwrap();
        assert_eq!(msgs.len(), 1, "the failed v1 table does not sink v2 rows");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn go_windows_only_use_dbs_matching_the_login() {
        let dir = tmp_dir("go-scope");
        let now = chrono::Utc::now().timestamp_millis();
        {
            let conn = full_db(&dir.join("opencode.db"), Some("test-key-a"));
            add_session_message(&conn, "a", "assistant", now, &v2_blob("opencode-go", "k3", 5.0, now));
        }
        {
            // A second channel logged into a DIFFERENT Go account.
            let conn = full_db(&dir.join("opencode-next.db"), Some("test-key-b"));
            add_session_message(&conn, "b", "assistant", now, &v2_blob("opencode-go", "k3", 7.0, now));
        }
        {
            // A third channel where Go was logged out (imported, no row).
            let conn = full_db(&dir.join("opencode-zeta.db"), None);
            add_session_message(&conn, "z", "assistant", now, &v2_blob("opencode-go", "k3", 11.0, now));
        }
        // A pre-import channel inherits auth.json's identity.
        write_auth(&dir, r#"{"opencode-go":{"type":"api","key":"test-key-a"}}"#);
        {
            let conn = make_db(&dir.join("opencode-old.db"), LEDGER_SCHEMA);
            conn.execute_batch(CRED_SCHEMA).unwrap(); // tables exist, no migration row
            add_session_message(&conn, "o", "assistant", now, &v2_blob("opencode-go", "k3", 13.0, now));
        }
        let go_cost = |dir: &Path, key: &str| -> f64 {
            read_messages(dir, key)
                .unwrap()
                .0
                .iter()
                .filter(|m| m.provider == "opencode-go")
                .map(|m| m.cost)
                .sum()
        };
        // key-a: opencode.db + the auth.json-matching old channel.
        assert!((go_cost(&dir, "test-key-a") - (5.0 + 13.0)).abs() < 1e-9);
        // key-b: only opencode-next.db — logged-out and foreign channels out.
        assert!((go_cost(&dir, "test-key-b") - 7.0).abs() < 1e-9);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn db_discovery_ignores_wal_sidecars_and_sorts() {
        let dir = tmp_dir("dbs");
        std::fs::write(dir.join("opencode.db"), b"").unwrap();
        std::fs::write(dir.join("opencode.db-wal"), b"").unwrap();
        std::fs::write(dir.join("opencode.db-shm"), b"").unwrap();
        std::fs::write(dir.join("opencode-next.db"), b"").unwrap();
        std::fs::write(dir.join("other.db"), b"").unwrap();
        let names: Vec<String> = opencode_dbs(&dir)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["opencode-next.db", "opencode.db"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn active_credential_row_wins_over_inactive_and_older() {
        let dir = tmp_dir("cred-active");
        {
            let conn = cred_db(&dir.join("opencode.db"), true);
            add_credential(&conn, "opencode-go", r#"{"type":"api","key":"test-key-inactive"}"#, 0, 3000);
            add_credential(&conn, "opencode-go", r#"{"type":"api","key":"test-key-a"}"#, 1, 1000);
            add_credential(&conn, "opencode-go", r#"{"type":"api","key":"test-key-b"}"#, 1, 2000);
        }
        // active DESC, then time_updated DESC — the newer active row.
        assert_eq!(
            auth_entry_key_in(&dir, "opencode-go").as_deref(),
            Some("test-key-b")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn imported_db_without_the_integration_stays_logged_out() {
        let dir = tmp_dir("cred-logout");
        {
            let conn = cred_db(&dir.join("opencode.db"), true);
            add_credential(&conn, "openrouter", r#"{"type":"api","key":"test-key-or"}"#, 1, 1000);
        }
        // auth.json still carries the old key — it must not revive a
        // v2 logout now that the credential table is authoritative.
        write_auth(&dir, r#"{"opencode-go":{"type":"api","key":"stale-file-key"}}"#);
        assert_eq!(auth_entry_key_in(&dir, "opencode-go"), None);
        // …but an integration that IS imported resolves from the DB.
        assert_eq!(
            auth_entry_key_in(&dir, "openrouter").as_deref(),
            Some("test-key-or")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pre_import_db_falls_back_to_auth_json() {
        let dir = tmp_dir("cred-v1");
        {
            // credential rows exist but the import migration never ran —
            // a pre-OpenCode-2 store: auth.json stays authoritative.
            let conn = cred_db(&dir.join("opencode.db"), false);
            add_credential(&conn, "opencode-go", r#"{"type":"api","key":"test-key-db"}"#, 1, 1000);
        }
        write_auth(&dir, r#"{"opencode-go":{"type":"api","key":"test-key-file"}}"#);
        assert_eq!(
            auth_entry_key_in(&dir, "opencode-go").as_deref(),
            Some("test-key-file")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn second_channel_db_supplies_the_key() {
        let dir = tmp_dir("cred-second");
        {
            // opencode-next.db sorts first: imported, no opencode-go row.
            let conn = cred_db(&dir.join("opencode-next.db"), true);
            add_credential(&conn, "moonshotai", r#"{"type":"api","key":"test-key-mm"}"#, 1, 1000);
        }
        {
            let conn = cred_db(&dir.join("opencode.db"), true);
            add_credential(&conn, "opencode-go", r#"{"type":"api","key":"test-key-a"}"#, 1, 1000);
        }
        assert_eq!(
            auth_entry_key_in(&dir, "opencode-go").as_deref(),
            Some("test-key-a")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn openrouter_and_aihubmix_resolve_through_the_same_path() {
        let dir = tmp_dir("cred-entries");
        {
            let conn = cred_db(&dir.join("opencode.db"), true);
            add_credential(&conn, "openrouter", r#"{"type":"api","key":"test-key-or"}"#, 1, 1000);
            add_credential(&conn, "aihubmix", r#"{"type":"api","key":"test-key-mix"}"#, 1, 1000);
        }
        assert_eq!(
            auth_entry_key_in(&dir, "openrouter").as_deref(),
            Some("test-key-or")
        );
        assert_eq!(
            auth_entry_key_in(&dir, "aihubmix").as_deref(),
            Some("test-key-mix")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dir_auth_state_reflects_a_db_logout_not_missing() {
        let dir = tmp_dir("cred-state");
        {
            let conn = cred_db(&dir.join("opencode.db"), true);
            add_credential(&conn, "openrouter", r#"{"type":"api","key":"test-key-or"}"#, 1, 1000);
        }
        // Imported store, no Go key → ParsedNoGo (a real logout), not
        // Missing — and an identified key fingerprints as before.
        assert!(matches!(dir_auth_state(&dir), DirAuth::ParsedNoGo));
        {
            let conn = rusqlite::Connection::open(dir.join("opencode.db")).unwrap();
            add_credential(&conn, "opencode-go", r#"{"type":"api","key":"test-key-go"}"#, 1, 1000);
        }
        assert!(matches!(dir_auth_state(&dir), DirAuth::Identified(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn incremental_reads_only_fetch_touched_rows() {
        let dir = tmp_dir("incr");
        let db = dir.join("opencode.db");
        let now = chrono::Utc::now().timestamp_millis();
        {
            let conn = make_db(&db, LEDGER_SCHEMA);
            add_session_message(&conn, "done", "assistant", now - 1000, &v2_blob("opencode-go", "k3", 1.0, now - 1000));
            // In-progress row: parses, but no cost/tokens — cached as None.
            add_session_message(
                &conn,
                "wip",
                "assistant",
                now - 500,
                &serde_json::json!({"model": {"id": "m", "providerID": "opencode-go"},
                                    "time": {"created": now - 500}})
                .to_string(),
            );
        }
        assert_eq!(collect_cost_events_in(&dir).len(), 1);

        // Nothing touched since the watermark → the incremental query
        // fetches zero rows (not a whole-table rescan).
        {
            let conn = crate::providers::open_readonly_sqlite(&db).unwrap();
            let cutoff = table_cutoff(
                &conn,
                "session_message",
                crate::spend::spend_cutoff_ms(chrono::Local::now()),
            );
            let mut map = HashMap::new();
            let (scanned, _) =
                scan_incremental(&conn, "session_message", true, cutoff, now - 499, &mut map)
                    .unwrap();
            assert_eq!(scanned, 0, "untouched rows are not refetched");
        }

        // wip finishes — OpenCode bumps time_updated on completion, so
        // the watermark catches it and its cost replaces the None slot.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute(
                "UPDATE session_message SET data=?1, time_updated=?2 WHERE id='wip'",
                rusqlite::params![v2_blob("opencode-go", "k3", 3.0, now), now + 1000],
            )
            .unwrap();
        }
        let rows = collect_cost_events_in(&dir);
        assert_eq!(rows.len(), 2, "the finished row replaces its placeholder");
        let total: f64 = rows.iter().map(|r| r.1).sum();
        assert!((total - 4.0).abs() < 1e-9);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn deleted_row_forces_a_table_rescan() {
        let dir = tmp_dir("incr-del");
        let db = dir.join("opencode.db");
        let now = chrono::Utc::now().timestamp_millis();
        {
            let conn = make_db(&db, LEDGER_SCHEMA);
            add_session_message(&conn, "a", "assistant", now, &v2_blob("opencode-go", "k3", 1.0, now));
            add_session_message(&conn, "b", "assistant", now, &v2_blob("opencode-go", "k3", 2.0, now));
        }
        assert_eq!(collect_cost_events_in(&dir).len(), 2);
        {
            // Deleting touches no time_updated — only the count check
            // can see it, and it must force a rescan.
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute("DELETE FROM session_message WHERE id='b'", []).unwrap();
        }
        let rows = collect_cost_events_in(&dir);
        assert_eq!(rows.len(), 1, "count mismatch rescans the table");
        assert!((rows[0].1 - 1.0).abs() < 1e-9);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn incremental_result_matches_a_fresh_scan() {
        let dir = tmp_dir("incr-eq");
        let db = dir.join("opencode.db");
        let now = chrono::Utc::now().timestamp_millis();
        {
            let conn = make_db(&db, LEDGER_SCHEMA);
            add_session_message(&conn, "v2a", "assistant", now - 100, &v2_blob("opencode-go", "k3", 2.0, now - 100));
            add_session_message(&conn, "v2b", "assistant", now - 200, &v2_blob("openrouter", "m", 0.5, now - 200));
            add_message(&conn, "v1a", now - 300, &v1_blob("opencode-go", "k3", 1.5, now - 300));
            add_message(&conn, "v1u", now - 400, r#"{"role":"user"}"#);
        }
        let first = collect_cost_events_in(&dir);
        // Touch the db without changing rows — stamps change, the
        // watermark path runs, and the result must be identical.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch("PRAGMA user_version=7;").unwrap();
        }
        let second = collect_cost_events_in(&dir);
        let key = |v: Vec<(f64, f64, f64, String, String)>| -> Vec<(i64, i64, String)> {
            let mut k: Vec<_> = v
                .into_iter()
                .map(|r| ((r.1 * 1e9) as i64, (r.2 * 1e9) as i64, r.4))
                .collect();
            k.sort();
            k
        };
        assert_eq!(first.len(), 3);
        assert_eq!(key(first), key(second));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn v2_content_is_stripped_and_malformed_rows_skip() {
        let dir = tmp_dir("content");
        let db = dir.join("opencode.db");
        let now = chrono::Utc::now().timestamp_millis();
        {
            let conn = make_db(&db, LEDGER_SCHEMA);
            // The transcript lives in `content` — SQL strips it before
            // the blob reaches the parser; the row still counts.
            let mut b: Value =
                serde_json::from_str(&v2_blob("opencode-go", "k3", 2.0, now)).unwrap();
            b["content"] =
                serde_json::json!([{"type": "tool_result", "output": "x".repeat(200_000)}]);
            add_session_message(&conn, "big", "assistant", now, &b.to_string());
            // Malformed JSON → json_valid=0 → NULL → skipped, neighbours
            // still count.
            add_session_message(&conn, "bad", "assistant", now, "{not-json");
            add_session_message(&conn, "ok", "assistant", now, &v2_blob("opencode-go", "k3", 1.0, now));
        }
        let rows = collect_cost_events_in(&dir);
        assert_eq!(rows.len(), 2);
        let total: f64 = rows.iter().map(|r| r.1).sum();
        assert!((total - 3.0).abs() < 1e-9);
        std::fs::remove_dir_all(&dir).ok();
    }
}