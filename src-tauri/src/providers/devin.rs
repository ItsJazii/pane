use super::{http_no_redirect, Metric, Snapshot};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;

const ID: &str = "devin";
const NAME: &str = "Devin";
// Mirrors the Mac app's DevinUsageClient — the server expects an IDE-shaped
// client identity and the Connect RPC protocol header.
const COMPAT_VERSION: &str = "1.108.2";
/// The vendor default — the only host that may receive the API key.
const DEFAULT_SERVER_URL: &str = "https://server.codeium.com";

/// The API key rides every status request; a planted api_server_url in
/// credentials.toml would exfiltrate it. Vendor host over https only.
fn is_trusted_server(url: &str) -> bool {
    reqwest::Url::parse(url)
        .map(|u| u.scheme() == "https" && u.host_str() == Some("server.codeium.com"))
        .unwrap_or(false)
}

fn credentials_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(appdata) = std::env::var("APPDATA") {
        paths.push(PathBuf::from(appdata).join("devin").join("credentials.toml"));
    }
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".local").join("share").join("devin").join("credentials.toml"));
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        paths.push(PathBuf::from(local).join("devin").join("credentials.toml"));
    }
    paths
}

/// Devin sends some numbers as JSON strings ("5000000") — accept both.
fn as_num(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    v.as_f64().or_else(|| v.as_str()?.trim().parse().ok())
}

/// A quota percent that proto3 omitted (zero value) reads as 0% remaining
/// when its sibling reset timestamp proves the quota exists.
fn zero_when_omitted(remaining: Option<f64>, reset: Option<f64>) -> Option<f64> {
    remaining.or_else(|| reset.is_some().then_some(0.0))
}

pub async fn snapshot() -> Snapshot {
    match fetch().await {
        Ok(s) => s,
        Err(e) => Snapshot::error(ID, NAME, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omitted_quota_percent_with_reset_means_exhausted() {
        // Exhausted week: percent omitted, reset present → 0% remaining.
        assert_eq!(zero_when_omitted(None, Some(1_786_262_400.0)), Some(0.0));
        // Percent present → passes through untouched.
        assert_eq!(zero_when_omitted(Some(37.0), Some(1.0)), Some(37.0));
        // Neither field → genuinely no such quota window.
        assert_eq!(zero_when_omitted(None, None), None);
    }

    fn temp_db(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "pane-devin-test-{name}-{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// A session store shaped like the real one: AUTOINCREMENT rowid
    /// (aliased by row_id), one row per message per branch, and the
    /// refinery history row that stamps the db's lineage (birth).
    fn create_db(path: &std::path::Path, birth: &str) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, model TEXT);
             CREATE TABLE message_nodes (
                 row_id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT NOT NULL,
                 chat_message TEXT NOT NULL,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE refinery_schema_history (
                 version INTEGER PRIMARY KEY,
                 name TEXT,
                 applied_on TEXT,
                 checksum TEXT
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO refinery_schema_history VALUES (1, 'initial_schema', ?1, '')",
            rusqlite::params![birth],
        )
        .unwrap();
        conn
    }

    fn insert_message(conn: &rusqlite::Connection, session: &str, chat_message: &str, created_s: i64) {
        conn.execute(
            "INSERT INTO message_nodes (session_id, chat_message, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![session, chat_message, created_s],
        )
        .unwrap();
    }

    fn assistant_msg(mid: &str, input: u32, output: u32) -> String {
        format!(
            "{{\"role\":\"assistant\",\"message_id\":\"{mid}\",\"metadata\":{{\"metrics\":{{\"input_tokens\":{input},\"output_tokens\":{output},\"cache_read_tokens\":0,\"cache_creation_tokens\":0}}}}}}"
        )
    }

    fn fresh_cache() -> DevinCache {
        DevinCache {
            version: DEVIN_CACHE_VERSION,
            ..Default::default()
        }
    }

    fn now_s() -> i64 {
        chrono::Utc::now().timestamp()
    }

    #[test]
    fn usage_events_read_live_file_read_only() {
        let path = temp_db("read-only");
        let conn = create_db(&path, "2026-05-30T00:00:00Z");
        conn.execute("INSERT INTO sessions VALUES ('s1', 'claude-sonnet-4')", [])
            .unwrap();
        // No metadata.created_at: ts falls back to the node clock.
        insert_message(&conn, "s1", &assistant_msg("m1", 10, 4), now_s() - 3600);
        drop(conn);

        let mut cache = fresh_cache();
        let outcome = refresh_cache(&path, &mut cache).expect("read-only live query");
        let _ = std::fs::remove_file(&path);
        assert_eq!(outcome, Refresh::Complete);
        assert_eq!(cache.events.len(), 1);
        assert_eq!(cache.events[0].input, 10.0);
        assert_eq!(cache.events[0].output, 4.0);
        assert_eq!(cache.events[0].model, "claude-sonnet-4");
        assert_eq!(cache.last_rowid, 1);
    }

    #[test]
    fn devin_incremental_read_dedups_and_only_reads_new_rows() {
        let path = temp_db("incremental");
        let conn = create_db(&path, "2026-05-30T00:00:00Z");
        conn.execute("INSERT INTO sessions VALUES ('s1', 'claude-sonnet-4')", [])
            .unwrap();
        insert_message(&conn, "s1", &assistant_msg("m1", 10, 4), now_s() - 3600);
        insert_message(
            &conn,
            "s1",
            "{\"role\":\"user\",\"content\":\"x\",\"metadata\":{\"metrics\":null}}",
            now_s() - 3600,
        );
        let mut cache = fresh_cache();
        assert_eq!(refresh_cache(&path, &mut cache), Ok(Refresh::Complete));
        assert_eq!(cache.events.len(), 1);
        assert_eq!(cache.last_rowid, 2);
        assert_eq!(cache.db_identity, "2026-05-30T00:00:00Z");

        // Same message on another branch (identical metrics) dedupes;
        // a new message joins with its own generation_model.
        insert_message(&conn, "s1", &assistant_msg("m1", 10, 4), now_s() - 3600);
        insert_message(
            &conn,
            "s1",
            "{\"role\":\"assistant\",\"message_id\":\"m2\",\"metadata\":{\"generation_model\":\"gpt-5\",\"metrics\":{\"input_tokens\":7,\"output_tokens\":3,\"cache_read_tokens\":0,\"cache_creation_tokens\":0}}}",
            now_s() - 3600,
        );
        assert_eq!(refresh_cache(&path, &mut cache), Ok(Refresh::Complete));
        assert_eq!(cache.events.len(), 2);
        let m2 = cache.events.iter().find(|e| e.mid == "m2").unwrap();
        assert_eq!(m2.model, "gpt-5");
        assert_eq!(cache.last_rowid, 4);

        // A db the cache already caught up with is a no-op.
        assert_eq!(refresh_cache(&path, &mut cache), Ok(Refresh::Unchanged));
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn devin_cache_rescans_when_rowids_restart() {
        let path = temp_db("restart");
        let conn = create_db(&path, "2026-05-30T00:00:00Z");
        conn.execute("INSERT INTO sessions VALUES ('s1', 'claude-sonnet-4')", [])
            .unwrap();
        insert_message(&conn, "s1", &assistant_msg("a", 1, 1), now_s() - 3600);
        insert_message(&conn, "s1", &assistant_msg("b", 2, 2), now_s() - 3600);
        drop(conn);

        // Same lineage, but the cache's rowid mark is past the db's
        // AUTOINCREMENT high-water — the file is younger than the cache.
        let mut cache = DevinCache {
            version: DEVIN_CACHE_VERSION,
            db_identity: "2026-05-30T00:00:00Z".into(),
            last_rowid: 100,
            events: vec![StoredEvent {
                ts_ms: chrono::Utc::now().timestamp_millis(),
                model: "stale".into(),
                input: 9.0,
                output: 9.0,
                cache_read: 0.0,
                cache_write: 0.0,
                session_id: "old".into(),
                mid: "stale".into(),
            }],
        };
        assert_eq!(refresh_cache(&path, &mut cache), Ok(Refresh::Complete));
        let _ = std::fs::remove_file(&path);
        assert_eq!(cache.last_rowid, 2);
        assert_eq!(cache.events.len(), 2);
        assert!(cache.events.iter().all(|e| e.session_id == "s1"));
    }

    #[test]
    fn devin_cache_rescans_when_db_is_replaced() {
        let path = temp_db("replaced");
        let conn = create_db(&path, "A");
        conn.execute("INSERT INTO sessions VALUES ('s1', 'claude-sonnet-4')", [])
            .unwrap();
        insert_message(&conn, "s1", &assistant_msg("m1", 1, 1), now_s() - 3600);
        insert_message(&conn, "s1", &assistant_msg("m2", 2, 2), now_s() - 3600);
        let mut cache = fresh_cache();
        assert_eq!(refresh_cache(&path, &mut cache), Ok(Refresh::Complete));
        assert_eq!(cache.events.len(), 2);
        assert_eq!(cache.last_rowid, 2);
        assert_eq!(cache.db_identity, "A");
        drop(conn);

        // Same path, different lineage: Bob's db replaces Alice's.
        super::super::remove_sqlite_files(&path);
        let conn = create_db(&path, "B");
        conn.execute("INSERT INTO sessions VALUES ('s1', 'claude-sonnet-4')", [])
            .unwrap();
        insert_message(&conn, "s1", &assistant_msg("n1", 1, 1), now_s() - 3600);
        insert_message(&conn, "s1", &assistant_msg("n2", 2, 2), now_s() - 3600);
        insert_message(&conn, "s1", &assistant_msg("n3", 3, 3), now_s() - 3600);
        drop(conn);

        assert_eq!(refresh_cache(&path, &mut cache), Ok(Refresh::Complete));
        let _ = std::fs::remove_file(&path);
        assert_eq!(cache.db_identity, "B");
        assert_eq!(cache.last_rowid, 3);
        assert_eq!(cache.events.len(), 3);
        let mut mids: Vec<&str> = cache.events.iter().map(|e| e.mid.as_str()).collect();
        mids.sort();
        assert_eq!(mids, ["n1", "n2", "n3"]);
    }

    #[test]
    fn devin_deleted_sessions_keep_counted_spend() {
        let path = temp_db("deleted");
        let conn = create_db(&path, "2026-05-30T00:00:00Z");
        conn.execute("INSERT INTO sessions VALUES ('s1', 'claude-sonnet-4')", [])
            .unwrap();
        insert_message(&conn, "s1", &assistant_msg("m1", 1, 1), now_s() - 3600);
        insert_message(&conn, "s1", &assistant_msg("m2", 2, 2), now_s() - 3600);
        insert_message(&conn, "s1", &assistant_msg("m3", 3, 3), now_s() - 3600);
        let mut cache = fresh_cache();
        assert_eq!(refresh_cache(&path, &mut cache), Ok(Refresh::Complete));
        assert_eq!(cache.events.len(), 3);
        assert_eq!(cache.last_rowid, 3);

        // `devin rm` pulls MAX(rowid) below the mark, but the
        // AUTOINCREMENT high-water stays — a deletion is not a
        // recreation, and counted spend stays until it ages out.
        conn.execute("DELETE FROM message_nodes WHERE rowid = 3", [])
            .unwrap();
        assert_eq!(refresh_cache(&path, &mut cache), Ok(Refresh::Unchanged));
        drop(conn);
        let _ = std::fs::remove_file(&path);
        assert_eq!(cache.events.len(), 3);
        assert_eq!(cache.last_rowid, 3);
    }

    #[test]
    fn devin_row_cap_resumes_without_gaps() {
        let path = temp_db("rowcap");
        let conn = create_db(&path, "2026-05-30T00:00:00Z");
        conn.execute("INSERT INTO sessions VALUES ('s1', 'claude-sonnet-4')", [])
            .unwrap();
        for i in 1u32..=5 {
            insert_message(
                &conn,
                "s1",
                &assistant_msg(&format!("m{i}"), i, i),
                now_s() - 3600,
            );
        }
        drop(conn);

        let mut cache = fresh_cache();
        // Each pass reads at most 2 rows and resumes where it stopped —
        // the rows past the cap are picked up, never skipped.
        assert_eq!(
            refresh_cache_with_limit(&path, &mut cache, 2),
            Ok(Refresh::Capped)
        );
        assert_eq!(cache.events.len(), 2);
        assert_eq!(cache.last_rowid, 2);
        assert_eq!(
            refresh_cache_with_limit(&path, &mut cache, 2),
            Ok(Refresh::Capped)
        );
        assert_eq!(cache.events.len(), 4);
        assert_eq!(cache.last_rowid, 4);
        assert_eq!(
            refresh_cache_with_limit(&path, &mut cache, 2),
            Ok(Refresh::Complete)
        );
        assert_eq!(cache.events.len(), 5);
        assert_eq!(cache.last_rowid, 5);
        assert_eq!(
            refresh_cache_with_limit(&path, &mut cache, 2),
            Ok(Refresh::Unchanged)
        );
        let _ = std::fs::remove_file(&path);
        let mut mids: Vec<&str> = cache.events.iter().map(|e| e.mid.as_str()).collect();
        mids.sort();
        assert_eq!(mids, ["m1", "m2", "m3", "m4", "m5"]);
    }

    #[test]
    fn devin_capped_pass_keeps_refreshing_unchanged_db() {
        let path = temp_db("capstall");
        let conn = create_db(&path, "2026-05-30T00:00:00Z");
        conn.execute("INSERT INTO sessions VALUES ('s1', 'claude-sonnet-4')", [])
            .unwrap();
        for i in 1u32..=5 {
            insert_message(
                &conn,
                "s1",
                &assistant_msg(&format!("m{i}"), i, i),
                now_s() - 3600,
            );
        }
        drop(conn);
        let sentinel: FileStamp = (std::time::UNIX_EPOCH, 0);
        let mut state = DevinState {
            db_stamp: sentinel,
            wal_stamp: sentinel,
            cache: fresh_cache(),
        };
        // The db never changes between calls; each capped pass must still
        // resume instead of being short-circuited by the stamp fast path.
        assert_eq!(collect_with(&path, &mut state, None, 2).len(), 2);
        assert_eq!(state.db_stamp, sentinel);
        assert_eq!(collect_with(&path, &mut state, None, 2).len(), 4);
        assert_eq!(state.db_stamp, sentinel);
        assert_eq!(collect_with(&path, &mut state, None, 2).len(), 5);
        // Caught up: stamps recorded, next call takes the fast path.
        assert_eq!(state.db_stamp, file_stamp(&path));
        assert_eq!(collect_with(&path, &mut state, None, 2).len(), 5);
        assert_eq!(state.cache.last_rowid, 5);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn devin_cache_roundtrip_and_version_gate() {
        let path = std::env::temp_dir().join(format!(
            "pane-devin-test-cache-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        let mut cache = fresh_cache();
        cache.db_identity = "2026-05-30T00:00:00Z".into();
        cache.last_rowid = 7;
        cache.events.push(StoredEvent {
            ts_ms: 123,
            model: "claude-sonnet-4".into(),
            input: 10.0,
            output: 4.0,
            cache_read: 1.0,
            cache_write: 2.0,
            session_id: "s1".into(),
            mid: "m1".into(),
        });
        save_cache(&path, &cache);
        let loaded = load_cache(&path).expect("roundtrip");
        assert_eq!(loaded.db_identity, "2026-05-30T00:00:00Z");
        assert_eq!(loaded.last_rowid, 7);
        assert_eq!(loaded.events.len(), 1);
        assert_eq!(loaded.events[0].input, 10.0);
        assert_eq!(loaded.events[0].mid, "m1");

        // A file from another cache version is ignored.
        std::fs::write(
            &path,
            "{\"version\":999,\"db_identity\":\"x\",\"last_rowid\":1,\"events\":[]}",
        )
        .unwrap();
        assert!(load_cache(&path).is_none());
        let _ = std::fs::remove_file(&path);
        // Missing file is a cold start, not an error.
        assert!(load_cache(&path).is_none());
    }

    #[test]
    fn devin_save_cache_creates_missing_config_dir() {
        let dir = std::env::temp_dir().join(format!(
            "pane-devin-test-cachedir-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("devin_spend_cache.json");
        // Fresh install: %APPDATA%\Pane may not exist yet when the first
        // scan persists — the save must not silently drop the bookmark.
        let mut cache = fresh_cache();
        cache.last_rowid = 3;
        save_cache(&path, &cache);
        let loaded = load_cache(&path).expect("saved into a created dir");
        assert_eq!(loaded.last_rowid, 3);
        assert!(!path.with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn devin_malformed_chat_message_is_skipped() {
        let path = temp_db("malformed");
        let conn = create_db(&path, "2026-05-30T00:00:00Z");
        conn.execute("INSERT INTO sessions VALUES ('s1', 'claude-sonnet-4')", [])
            .unwrap();
        insert_message(&conn, "s1", "not json", now_s() - 3600);
        insert_message(&conn, "s1", &assistant_msg("m1", 10, 4), now_s() - 3600);
        drop(conn);

        let mut cache = fresh_cache();
        assert_eq!(refresh_cache(&path, &mut cache), Ok(Refresh::Complete));
        let _ = std::fs::remove_file(&path);
        assert_eq!(cache.events.len(), 1);
        assert_eq!(cache.last_rowid, 2);
    }

    #[test]
    fn devin_prunes_events_older_than_window() {
        let mut cache = fresh_cache();
        cache.events.push(StoredEvent {
            ts_ms: chrono::Utc::now().timestamp_millis() - 40 * 86_400 * 1_000,
            model: "claude-sonnet-4".into(),
            input: 1.0,
            output: 1.0,
            cache_read: 0.0,
            cache_write: 0.0,
            session_id: "s1".into(),
            mid: "old".into(),
        });
        merge_new_events(&mut cache, Vec::new(), 5);
        assert!(cache.events.is_empty());
        assert_eq!(cache.last_rowid, 5);
    }

    /// Live diagnostic (ignored): prints the raw planStatus so quota
    /// misreports can be debugged against real account states (never
    /// prints the key). Run:
    ///   cargo test devin_status_live_probe -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn devin_status_live_probe() {
        let Some(path) = credentials_paths().into_iter().find(|p| p.exists()) else {
            println!("no credentials.toml");
            return;
        };
        let raw = std::fs::read_to_string(&path).unwrap();
        let doc: toml::Value = toml::from_str(&raw).unwrap();
        let api_key = doc.get("windsurf_api_key").and_then(toml::Value::as_str).unwrap();
        let server = doc
            .get("api_server_url")
            .and_then(toml::Value::as_str)
            .unwrap_or("https://server.codeium.com")
            .trim_end_matches('/');
        if !is_trusted_server(server) {
            panic!("credentials.toml api_server_url is not {DEFAULT_SERVER_URL} — refusing to send the key");
        }
        let resp = http_no_redirect()
            .post(format!("{server}/exa.seat_management_pb.SeatManagementService/GetUserStatus"))
            .header("Content-Type", "application/json")
            .header("Connect-Protocol-Version", "1")
            .json(&json!({ "metadata": { "apiKey": api_key, "ideName": "devin",
                "ideVersion": COMPAT_VERSION, "extensionName": "devin",
                "extensionVersion": COMPAT_VERSION, "locale": "en" } }))
            .send()
            .await
            .unwrap();
        println!("status: {}", resp.status());
        let body: Value = resp.json().await.unwrap();
        let dump = serde_json::to_string_pretty(
            body.pointer("/userStatus/planStatus").unwrap_or(&Value::Null),
        )
        .unwrap();
        let dump: String = dump.chars().take(200).collect();
        println!("planStatus: {dump}");
    }
}

async fn fetch() -> Result<Snapshot, String> {
    let Some(path) = credentials_paths().into_iter().find(|p| p.exists()) else {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Devin CLI sign-in not found (credentials.toml).",
        ));
    };

    let raw = std::fs::read_to_string(&path).map_err(|e| format!("read credentials.toml: {e}"))?;
    let doc: toml::Value = toml::from_str(&raw).map_err(|e| format!("parse credentials.toml: {e}"))?;
    let api_key = doc
        .get("windsurf_api_key")
        .and_then(toml::Value::as_str)
        .ok_or("credentials.toml has no windsurf_api_key")?
        .to_string();
    let server = doc
        .get("api_server_url")
        .and_then(toml::Value::as_str)
        .unwrap_or(DEFAULT_SERVER_URL)
        .trim_end_matches('/');
    if !is_trusted_server(server) {
        eprintln!(
            "[pane] devin: credentials.toml api_server_url is not {DEFAULT_SERVER_URL} — skipping status request"
        );
        return Err(
            "Devin credentials point at an unexpected server — sign in with the Devin CLI again".into(),
        );
    }
    let server = server.to_string();

    // The body carries the API key: never follow redirects — a 307/308
    // re-sends the body to the redirect target cross-origin.
    let resp = http_no_redirect()
        .post(format!(
            "{server}/exa.seat_management_pb.SeatManagementService/GetUserStatus"
        ))
        .header("Content-Type", "application/json")
        .header("Connect-Protocol-Version", "1")
        .json(&json!({
            "metadata": {
                "apiKey": api_key,
                "ideName": "devin",
                "ideVersion": COMPAT_VERSION,
                "extensionName": "devin",
                "extensionVersion": COMPAT_VERSION,
                "locale": "en",
            }
        }))
        .send()
        .await
        .map_err(|e| format!("status request: {e}"))?;
    if resp.status().as_u16() == 401 || resp.status().as_u16() == 403 {
        return Err("Devin credentials were rejected — sign in with the Devin CLI again".into());
    }
    if !resp.status().is_success() {
        return Err(format!("status endpoint: HTTP {}", resp.status()));
    }
    let body: Value = resp.json().await.map_err(|e| format!("status parse: {e}"))?;

    let plan_status = body
        .pointer("/userStatus/planStatus")
        .ok_or("response has no userStatus.planStatus")?;
    let plan_info = plan_status.get("planInfo").cloned().unwrap_or(Value::Null);

    let plan = plan_info
        .get("planName")
        .and_then(Value::as_str)
        .map(str::to_string);
    let hide_daily = plan_info
        .get("hideDailyQuota")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let daily_reset = as_num(plan_status.get("dailyQuotaResetAtUnix"));
    let weekly_reset = as_num(plan_status.get("weeklyQuotaResetAtUnix"));
    // proto3 JSON drops zero-valued fields: an exhausted quota loses its
    // RemainingPercent entirely while its reset timestamp stays. A missing
    // percent alongside a present reset therefore means 0% left — not "no
    // quota" — else a fully spent week rendered as a fresh 0%-used bar
    // (the same omitted-field trick Grok pulls with creditUsagePercent).
    let daily_remaining =
        zero_when_omitted(as_num(plan_status.get("dailyQuotaRemainingPercent")), daily_reset);
    let weekly_remaining =
        zero_when_omitted(as_num(plan_status.get("weeklyQuotaRemainingPercent")), weekly_reset);

    const DAY: i64 = 86_400_000;
    let to_ms = |unix: Option<f64>| unix.map(|s| (s * 1000.0) as i64);

    // Devin reports percent *remaining*; the meter shows percent *used*.
    let mut metrics = Vec::new();
    if !hide_daily {
        if let Some(remaining) = daily_remaining {
            metrics.push(
                Metric::progress("Daily", (100.0 - remaining).clamp(0.0, 100.0), None)
                    .with_reset(to_ms(daily_reset), Some(DAY)),
            );
        }
    }
    match (weekly_remaining, hide_daily, daily_remaining) {
        (Some(remaining), _, _) => {
            metrics.push(
                Metric::progress("Weekly", (100.0 - remaining).clamp(0.0, 100.0), None)
                    .with_reset(to_ms(weekly_reset), Some(7 * DAY)),
            );
        }
        // No weekly quota reported: surface the hidden daily quota in the
        // Weekly row so the card stays meaningful (same as the Mac app).
        (None, true, Some(remaining)) => {
            metrics.push(
                Metric::progress("Weekly", (100.0 - remaining).clamp(0.0, 100.0), None)
                    .with_reset(to_ms(weekly_reset), Some(7 * DAY)),
            );
        }
        _ => {}
    }
    if let Some(micros) = as_num(plan_status.get("overageBalanceMicros")) {
        let dollars = micros.max(0.0) / 1_000_000.0;
        // A funded balance meters like a plan window (against the highest
        // balance seen — a top-up raises it); an empty one stays a plain row.
        let meter = (dollars > 0.0)
            .then(|| super::credit_meter_labeled("devin-extra", "$", dollars, "Extra balance", ""))
            .flatten();
        match meter {
            Some(m) => metrics.push(m),
            None => metrics.push(Metric::text("Extra balance", format!("${dollars:.2}"))),
        }
    }

    if metrics.is_empty() {
        return Err("no quota data in response".into());
    }
    Ok(Snapshot::ok(ID, NAME, plan, metrics))
}

// ---------------------------------------------------------------------------
// Local spend events — the Devin CLI's sessions.db
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct UsageEvent {
    pub ts_ms: i64,
    pub model: String,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

fn sessions_db_path() -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    Some(PathBuf::from(appdata).join("devin").join("cli").join("sessions.db"))
}

/// (mtime, size) of one file; a fixed sentinel when it doesn't exist.
type FileStamp = (std::time::SystemTime, u64);

fn file_stamp(path: &std::path::Path) -> FileStamp {
    std::fs::metadata(path)
        .map(|m| (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len()))
        .unwrap_or((std::time::UNIX_EPOCH, 0))
}

/// Bump when the on-disk shape changes; a stale file rescan-resumes
/// from rowid 0 instead of trusting an old layout.
const DEVIN_CACHE_VERSION: u32 = 2;

/// One priced Devin message plus its dedup key. `mid` is "" when the
/// row carried neither message_id nor request_id (never deduped, as before).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct StoredEvent {
    ts_ms: i64,
    model: String,
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
    session_id: String,
    mid: String,
}

impl StoredEvent {
    fn to_usage(&self) -> UsageEvent {
        UsageEvent {
            ts_ms: self.ts_ms,
            model: self.model.clone(),
            input: self.input,
            output: self.output,
            cache_read: self.cache_read,
            cache_write: self.cache_write,
        }
    }
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct DevinCache {
    version: u32,
    /// Lineage stamp of the db these events came from. A replaced or
    /// recreated store reuses rowids, so last_rowid alone can't tell it
    /// apart — the identity can.
    db_identity: String,
    /// Highest message_nodes rowid processed. rowid is AUTOINCREMENT and
    /// rows are append-only, so anything past it is new — the WAL changes
    /// on every keystroke, but rowids only move when messages land.
    last_rowid: i64,
    events: Vec<StoredEvent>,
}

struct DevinState {
    db_stamp: FileStamp,
    wal_stamp: FileStamp,
    cache: DevinCache,
}

/// None until the on-disk cache has been loaded (first collect this run).
static STATE: Mutex<Option<DevinState>> = Mutex::new(None);

fn cache_path() -> PathBuf {
    super::config_dir().join("devin_spend_cache.json")
}

/// None when the file is missing, unparseable, or from another version.
fn load_cache(path: &std::path::Path) -> Option<DevinCache> {
    let raw = std::fs::read_to_string(path).ok()?;
    let cache: DevinCache = serde_json::from_str(&raw).ok()?;
    (cache.version == DEVIN_CACHE_VERSION).then_some(cache)
}

/// Atomic write via temp + rename (std::fs::rename replaces an existing
/// file on Windows too). Creates the config dir — a fresh install may
/// not have it yet. Failures are logged and just mean the next run
/// resumes from the last persisted rowid; the in-memory cache still
/// serves this run.
fn save_cache(path: &std::path::Path, cache: &DevinCache) {
    let Ok(json) = serde_json::to_string(cache) else { return };
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("[pane] devin: could not create {}: {e}", dir.display());
            return;
        }
    }
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, json) {
        eprintln!("[pane] devin: could not write {}: {e}", tmp.display());
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        eprintln!("[pane] devin: could not replace {}: {e}", path.display());
        let _ = std::fs::remove_file(&tmp);
    }
}

fn max_rowid(conn: &rusqlite::Connection) -> Result<i64, String> {
    conn.query_row("SELECT COALESCE(MAX(rowid), 0) FROM message_nodes", [], |r| {
        r.get(0)
    })
    .map_err(|e| format!("max rowid: {e}"))
}

/// Stable identity of the store's lineage: the moment the Devin CLI
/// applied its first migration. A recreated or swapped-in db gets a
/// new one. "" when the table is missing (old/odd stores still work).
fn db_identity(conn: &rusqlite::Connection) -> String {
    conn.query_row(
        "SELECT COALESCE(applied_on, '') FROM refinery_schema_history ORDER BY version LIMIT 1",
        [],
        |r| r.get::<_, String>(0),
    )
    .unwrap_or_default()
}

/// AUTOINCREMENT high-water mark for message_nodes. Unlike MAX(rowid)
/// it never drops when sessions are deleted, so a deletion is not
/// mistaken for a recreated store. Floored at MAX(rowid) for tables
/// without a sequence row.
fn rowid_high_water(conn: &rusqlite::Connection, max_rowid: i64) -> i64 {
    conn.query_row(
        "SELECT seq FROM sqlite_sequence WHERE name = 'message_nodes'",
        [],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
    .max(max_rowid)
}

/// sessions.db keeps one row per message per branch and can be GBs; cap
/// the rows one refresh reads so a bloated store can't pin a refresh.
/// The scan is oldest-first and resumes from the last rowid read, so a
/// store bigger than the cap is indexed across successive refreshes —
/// nothing is dropped. Real data is far below.
const MAX_MESSAGE_ROWS: usize = 2_000_000;

/// chat_message is a fat JSON blob — content/thinking/tool_calls carry
/// the bulk bytes. A typed parse lets serde skip those fields without
/// materialising them; only role/message_id/metadata are read.
#[derive(serde::Deserialize)]
struct ChatMessage {
    role: Option<Value>,
    message_id: Option<Value>,
    metadata: Option<Value>,
}

/// Read rows in `(after_rowid, up_to_rowid]` newer than `cutoff_s`,
/// oldest rowid first, at most `limit` rows. Returns the events, the
/// highest rowid returned (`after_rowid` when no rows came back), and
/// whether the cap was hit — the caller resumes from that rowid next
/// pass instead of skipping the rest. Any row error aborts the batch so
/// a half-read never advances last_rowid.
fn read_new_events(
    conn: &rusqlite::Connection,
    after_rowid: i64,
    up_to_rowid: i64,
    cutoff_s: i64,
    limit: usize,
) -> Result<(Vec<StoredEvent>, i64, bool), String> {
    let mut stmt = conn
        .prepare(
            "SELECT m.rowid, m.session_id, m.chat_message, m.created_at, s.model
             FROM message_nodes m JOIN sessions s ON s.id = m.session_id
             WHERE m.rowid > ?1 AND m.rowid <= ?2 AND m.created_at >= ?3
             ORDER BY m.rowid ASC
             LIMIT ?4",
        )
        .map_err(|e| format!("query messages: {e}"))?;
    let rows = stmt
        .query_map(
            rusqlite::params![after_rowid, up_to_rowid, cutoff_s, limit as i64],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .map_err(|e| format!("read messages: {e}"))?;

    let mut out = Vec::new();
    let mut scanned = 0usize;
    let mut last_read = after_rowid;
    for row in rows {
        scanned += 1;
        let (rowid, session_id, chat_message, node_created_s, model) =
            row.map_err(|e| format!("read message row: {e}"))?;
        last_read = rowid;
        let Ok(msg) = serde_json::from_str::<ChatMessage>(&chat_message) else { continue };
        if msg.role.as_ref().and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let md = msg.metadata.unwrap_or(Value::Null);
        let Some(metrics) = md.get("metrics").filter(|m| m.is_object()) else { continue };
        let mid = msg
            .message_id
            .as_ref()
            .and_then(Value::as_str)
            .or_else(|| md.get("request_id").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        let num = |k: &str| metrics.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        let ts_ms = md
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.timestamp_millis())
            .unwrap_or(node_created_s * 1000);
        // The message's own generation_model is the truth: the session-level
        // model is rewritten in place on every switch, which retroactively
        // relabels (and misprices) everything the session ran before. Older
        // records without the field fall back to the session model.
        let event_model = md
            .get("generation_model")
            .and_then(Value::as_str)
            .filter(|m| !m.trim().is_empty())
            .unwrap_or(&model)
            .to_string();
        out.push(StoredEvent {
            ts_ms,
            model: event_model,
            input: num("input_tokens"),
            output: num("output_tokens"),
            cache_read: num("cache_read_tokens"),
            cache_write: num("cache_creation_tokens"),
            session_id,
            mid,
        });
    }
    let capped = scanned >= limit;
    if capped {
        eprintln!("[pane] devin: read {limit} rows this pass — resuming next refresh");
    }
    Ok((out, last_read, capped))
}

/// Append a batch of new events onto the cache: one message can appear
/// on several branches of the session forest, so rows dedupe by
/// (session, message id) — first wins, and the batch is read
/// oldest-first so the earliest branch copy is the one kept (branch
/// copies carry identical metrics, so which one wins doesn't matter).
/// Then advance the high-water rowid and drop events that aged out of
/// the spend window so the file can't grow forever.
fn merge_new_events(cache: &mut DevinCache, new: Vec<StoredEvent>, up_to_rowid: i64) {
    let mut seen: std::collections::HashSet<(String, String)> = cache
        .events
        .iter()
        .filter(|e| !e.mid.is_empty())
        .map(|e| (e.session_id.clone(), e.mid.clone()))
        .collect();
    for ev in new {
        if !ev.mid.is_empty() && !seen.insert((ev.session_id.clone(), ev.mid.clone())) {
            continue;
        }
        cache.events.push(ev);
    }
    cache.last_rowid = up_to_rowid;
    let cutoff_ms = chrono::Utc::now().timestamp_millis() - 32 * 86_400 * 1_000;
    cache.events.retain(|e| e.ts_ms >= cutoff_ms);
}

/// Outcome of one incremental pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refresh {
    /// Nothing landed since last pass; nothing to persist.
    Unchanged,
    /// Caught up to the store's max rowid (rows processed, identity
    /// adopted, or a reset) — persist.
    Complete,
    /// Hit MAX_MESSAGE_ROWS; rows remain past last_rowid — persist, and
    /// the caller must not record the file stamps so the next refresh
    /// resumes even if the store didn't change.
    Capped,
}

/// Read only the rows past the remembered rowid. Refresh::Complete when
/// the pass caught up (rows processed, identity adopted, or a reset —
/// the caller persists), Refresh::Capped when the row limit cut the pass
/// short and rows remain past last_rowid, Refresh::Unchanged when
/// nothing landed.
#[cfg(test)]
fn refresh_cache(db: &std::path::Path, cache: &mut DevinCache) -> Result<Refresh, String> {
    refresh_cache_with_limit(db, cache, MAX_MESSAGE_ROWS)
}

fn refresh_cache_with_limit(
    db: &std::path::Path,
    cache: &mut DevinCache,
    limit: usize,
) -> Result<Refresh, String> {
    let conn = super::open_readonly_sqlite(db)?;
    let identity = db_identity(&conn);
    let max = max_rowid(&conn)?;
    let high_water = rowid_high_water(&conn, max);
    // A cache that never processed a row just adopts this store's
    // identity — starting empty is a cold start, not a replacement
    // (without this, a fresh cache's "" identity would trip the reset
    // check on every first pass).
    if cache.db_identity.is_empty() && cache.last_rowid == 0 {
        cache.db_identity = identity.clone();
    }
    // A swapped or recreated db can reuse rowids below the remembered
    // mark — the lineage stamp catches what last_rowid alone can't, and
    // a high-water regression means the file itself is younger than the
    // cache claims.
    let reset = cache.db_identity != identity || high_water < cache.last_rowid;
    if reset {
        eprintln!("[pane] devin: sessions.db was recreated or replaced — rescanning");
        *cache = DevinCache {
            version: DEVIN_CACHE_VERSION,
            db_identity: identity,
            ..Default::default()
        };
    }
    // Sessions deleted since the last pass can pull MAX(rowid) below the
    // mark; nothing new landed, and already-counted spend stays until it
    // ages out of the window.
    if max <= cache.last_rowid {
        return Ok(if reset {
            Refresh::Complete
        } else {
            Refresh::Unchanged
        });
    }
    // Node created_at is the index clock; attribution prefers metadata
    // created_at. One extra day of slack covers a straddle without
    // scanning the whole GB-sized sessions.db.
    let cutoff_s = chrono::Utc::now().timestamp() - 32 * 86_400;
    let (new, last_read, capped) =
        read_new_events(&conn, cache.last_rowid, max, cutoff_s, limit)?;
    // When the cap cut the batch short, resume from the highest rowid
    // read instead of max — the rest are picked up next refresh.
    merge_new_events(cache, new, if capped { last_read } else { max });
    Ok(if capped {
        Refresh::Capped
    } else {
        Refresh::Complete
    })
}

/// Per-request token metrics from the Devin CLI's local session store.
/// Assistant messages carry a metrics object (input/output/cache tokens);
/// the store keeps one row per message per branch of the session's message
/// forest, so rows dedupe by (session, message id). The model is tracked
/// per session. Cloud Devin sessions bill ACUs and never land in this db —
/// only CLI usage shows up.
///
/// The live file is often multiple GB and its WAL changes constantly, so
/// an mtime/size stamp misses on every refresh — each pass used to parse
/// the whole window of chat JSON (minutes of CPU on a busy machine).
/// Instead we persist the highest rowid processed to
/// `devin_spend_cache.json` and only read rows above it; the first scan
/// (or a stale/corrupt cache) still walks the window once.
///
/// Copying the db into `%TEMP%` via the backup API inherited WAL mode
/// and, when the dest journal survived a refresh, grew by another full
/// copy each cycle (tens of GB on C:). Readers in WAL mode already see a
/// consistent snapshot, so we query the live file read-only and never
/// write a temp copy.
pub fn collect_usage_events() -> Vec<UsageEvent> {
    super::sweep_temp_sqlite_prefix("pane-devin-");

    let Some(db_path) = sessions_db_path() else { return Vec::new() };
    if !db_path.exists() {
        return Vec::new();
    }

    let Ok(mut guard) = STATE.lock() else { return Vec::new() };
    let cache_file = cache_path();
    if guard.is_none() {
        // First collect this run: seed from disk (empty when missing or
        // stale) with sentinel stamps so we always refresh once — the
        // WAL almost certainly moved while Pane wasn't running.
        *guard = Some(DevinState {
            db_stamp: (std::time::UNIX_EPOCH, 0),
            wal_stamp: (std::time::UNIX_EPOCH, 0),
            cache: load_cache(&cache_file).unwrap_or(DevinCache {
                version: DEVIN_CACHE_VERSION,
                ..Default::default()
            }),
        });
    }
    let Some(state) = guard.as_mut() else { return Vec::new() };
    collect_with(&db_path, state, Some(&cache_file), MAX_MESSAGE_ROWS)
}

/// One collect against `db_path` with `state`. Stamps are recorded only
/// when the pass caught up (or nothing landed): a capped pass leaves them
/// stale so the next call resumes even if the store didn't change.
/// `cache_file` None skips persisting (tests).
fn collect_with(
    db_path: &std::path::Path,
    state: &mut DevinState,
    cache_file: Option<&std::path::Path>,
    limit: usize,
) -> Vec<UsageEvent> {
    let db_stamp = file_stamp(db_path);
    let wal_stamp = file_stamp(&db_path.with_extension("db-wal"));
    if state.db_stamp == db_stamp && state.wal_stamp == wal_stamp {
        return events_in_spend_window(&state.cache.events);
    }
    match refresh_cache_with_limit(db_path, &mut state.cache, limit) {
        Ok(outcome) => {
            if outcome != Refresh::Capped {
                state.db_stamp = db_stamp;
                state.wal_stamp = wal_stamp;
            }
            if outcome != Refresh::Unchanged {
                if let Some(path) = cache_file {
                    save_cache(path, &state.cache);
                }
            }
            events_in_spend_window(&state.cache.events)
        }
        // Busy/locked db: keep showing the last good events instead of a
        // sudden empty Devin row; the next refresh retries. Stamps and
        // last_rowid stay put so the retry re-attempts the same read.
        Err(_) => events_in_spend_window(&state.cache.events),
    }
}

fn events_in_spend_window(events: &[StoredEvent]) -> Vec<UsageEvent> {
    let cutoff_ms = chrono::Utc::now().timestamp_millis() - 32 * 86_400 * 1_000;
    events
        .iter()
        .filter(|e| e.ts_ms >= cutoff_ms)
        .map(StoredEvent::to_usage)
        .collect()
}
