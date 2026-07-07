use super::{http, Metric, Snapshot};
use serde_json::Value;
use std::path::PathBuf;

const ID: &str = "cursor";
const NAME: &str = "Cursor";

fn state_db_path() -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    let p = PathBuf::from(appdata)
        .join("Cursor")
        .join("User")
        .join("globalStorage")
        .join("state.vscdb");
    p.exists().then_some(p)
}

/// Cursor stores its session token in a small SQLite database. The running
/// Cursor app may hold a lock on it, so we read from a temporary copy.
fn read_state_values() -> Result<Option<String>, String> {
    let Some(db_path) = state_db_path() else {
        return Ok(None);
    };
    let tmp = std::env::temp_dir().join(format!("openusage-cursor-{}.vscdb", std::process::id()));
    std::fs::copy(&db_path, &tmp).map_err(|e| format!("copy state.vscdb: {e}"))?;

    let result = (|| -> Result<Option<String>, rusqlite::Error> {
        let conn = rusqlite::Connection::open_with_flags(
            &tmp,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let get = |key: &str| -> Result<Option<String>, rusqlite::Error> {
            match conn.query_row("SELECT value FROM ItemTable WHERE key = ?1", [key], |r| {
                r.get::<_, String>(0)
            }) {
                Ok(v) => Ok(Some(v)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e),
            }
        };
        get("cursorAuth/accessToken")
    })();

    let _ = std::fs::remove_file(&tmp);
    result.map_err(|e| format!("read state.vscdb: {e}"))
}

/// Values in ItemTable are sometimes stored as JSON strings ("\"abc\"").
fn unquote(v: &str) -> String {
    v.trim().trim_matches('"').to_string()
}

fn jwt_sub(token: &str) -> Option<String> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims.get("sub").and_then(Value::as_str).map(str::to_string)
}

pub async fn snapshot() -> Snapshot {
    match fetch().await {
        Ok(s) => s,
        Err(e) => Snapshot::error(ID, NAME, e),
    }
}

/// The dashboard's usage-events CSV export — the raw material for Cursor
/// spend tiles. Best-effort: any failure just means no Cursor spend row.
/// Cached for an hour; the export can be sizable and changes slowly.
pub async fn fetch_usage_csv() -> Option<String> {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<(i64, String)>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new((0, String::new())));
    let now = chrono::Utc::now().timestamp_millis();
    if let Ok(c) = cache.lock() {
        if now - c.0 < 3_600_000 && !c.1.is_empty() {
            return Some(c.1.clone());
        }
    }

    let token = unquote(&read_state_values().ok()??);
    if token.is_empty() {
        return None;
    }
    let sub = jwt_sub(&token)?;
    let user_id = sub.split('|').next_back().unwrap_or(&sub).to_string();
    let cookie = format!("WorkosCursorSessionToken={user_id}%3A%3A{token}");

    let resp = http()
        .get("https://cursor.com/api/dashboard/export-usage-events-csv")
        .header("Cookie", &cookie)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        eprintln!("[pane] cursor csv: HTTP {}", resp.status());
        return None;
    }
    let body = resp.text().await.ok()?;
    if body.trim().is_empty() {
        return None;
    }
    if let Ok(mut c) = cache.lock() {
        *c = (now, body.clone());
    }
    Some(body)
}

async fn fetch() -> Result<Snapshot, String> {
    let Some(token_raw) = read_state_values()? else {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Cursor sign-in not found. Open Cursor and log in.",
        ));
    };
    let token = unquote(&token_raw);
    if token.is_empty() {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Cursor sign-in not found. Open Cursor and log in.",
        ));
    }

    // Cursor's web session cookie is "<user_id>::<jwt>"; the user id is the
    // part of the JWT `sub` claim after the "auth0|" prefix.
    let sub = jwt_sub(&token).ok_or("could not decode Cursor session token")?;
    let user_id = sub.split('|').next_back().unwrap_or(&sub).to_string();
    let cookie = format!("WorkosCursorSessionToken={user_id}%3A%3A{token}");

    let usage_req = http()
        .get(format!("https://cursor.com/api/usage?user={user_id}"))
        .header("Cookie", &cookie)
        .send();
    let plan_req = http()
        .get("https://cursor.com/api/auth/stripe")
        .header("Cookie", &cookie)
        .send();
    let (usage_resp, plan_resp) = tokio::join!(usage_req, plan_req);

    let usage_resp = usage_resp.map_err(|e| format!("usage request: {e}"))?;
    if usage_resp.status().as_u16() == 401 || usage_resp.status().as_u16() == 403 {
        return Err("Cursor session expired — open Cursor once to refresh it".into());
    }
    if !usage_resp.status().is_success() {
        return Err(format!("usage endpoint: HTTP {}", usage_resp.status()));
    }
    let usage: Value = usage_resp.json().await.map_err(|e| format!("usage parse: {e}"))?;

    let mut plan: Option<String> = None;
    if let Ok(r) = plan_resp {
        if r.status().is_success() {
            if let Ok(v) = r.json::<Value>().await {
                plan = v
                    .get("membershipType")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
        }
    }

    let mut metrics = Vec::new();
    if let Some(gpt4) = usage.get("gpt-4") {
        let used = gpt4.get("numRequests").and_then(Value::as_f64).unwrap_or(0.0);
        match gpt4.get("maxRequestUsage").and_then(Value::as_f64) {
            Some(max) if max > 0.0 => {
                metrics.push(Metric::progress(
                    "Requests",
                    used / max * 100.0,
                    Some(format!("{used:.0} / {max:.0} this cycle")),
                ));
            }
            _ => {
                metrics.push(Metric::text("Requests this cycle", format!("{used:.0}")));
            }
        }
    }
    if metrics.is_empty() {
        return Err("usage response had no recognizable data".into());
    }
    Ok(Snapshot::ok(ID, NAME, plan, metrics))
}
