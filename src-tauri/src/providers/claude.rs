use super::{http, Metric, Snapshot};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::path::PathBuf;

// Claude Code's public OAuth client id — the same one the CLI itself uses.
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const ID: &str = "claude";
const NAME: &str = "Claude";

fn creds_path() -> PathBuf {
    let config_dir = std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".claude"));
    config_dir.join(".credentials.json")
}

pub async fn snapshot() -> Snapshot {
    match fetch().await {
        Ok(s) => s,
        Err(e) => Snapshot::error(ID, NAME, e),
    }
}

async fn fetch() -> Result<Snapshot, String> {
    let path = creds_path();
    if !path.exists() {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Claude Code sign-in not found. Run `claude` in a terminal and log in.",
        ));
    }

    let raw = std::fs::read_to_string(&path).map_err(|e| format!("read credentials: {e}"))?;
    let mut doc: Value = serde_json::from_str(&raw).map_err(|e| format!("parse credentials: {e}"))?;
    let oauth = doc
        .get("claudeAiOauth")
        .cloned()
        .ok_or("credentials file has no claudeAiOauth entry")?;

    let mut access = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let refresh = oauth
        .get("refreshToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let expires_at = oauth.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
    let plan = oauth
        .get("subscriptionType")
        .and_then(Value::as_str)
        .map(str::to_string);

    // Tokens go stale; swap the refresh token for a fresh access token when needed.
    let now_ms = Utc::now().timestamp_millis();
    if access.is_empty() || expires_at <= now_ms + 60_000 {
        if refresh.is_empty() {
            return Err("token expired and no refresh token present — run `claude` and log in again".into());
        }
        let resp = http()
            .post("https://platform.claude.com/v1/oauth/token")
            .json(&json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh,
                "client_id": CLIENT_ID,
            }))
            .send()
            .await
            .map_err(|e| format!("token refresh: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("token refresh failed: HTTP {}", resp.status()));
        }
        let tok: Value = resp.json().await.map_err(|e| format!("token refresh parse: {e}"))?;
        let new_access = tok
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or("refresh response missing access_token")?
            .to_string();
        let new_refresh = tok
            .get("refresh_token")
            .and_then(Value::as_str)
            .unwrap_or(&refresh)
            .to_string();
        let expires_in = tok.get("expires_in").and_then(Value::as_i64).unwrap_or(3600);

        access = new_access.clone();

        // Refresh tokens rotate on use — write the new pair back so Claude Code
        // itself stays logged in.
        if let Some(entry) = doc.get_mut("claudeAiOauth").filter(|v| v.is_object()) {
            entry["accessToken"] = Value::from(new_access);
            entry["refreshToken"] = Value::from(new_refresh);
            entry["expiresAt"] = Value::from(now_ms + expires_in * 1000);
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_string_pretty(&doc).unwrap_or(raw))
                .and_then(|_| std::fs::rename(&tmp, &path))
                .map_err(|e| format!("write refreshed credentials: {e}"))?;
        }
    }

    let resp = http()
        .get("https://api.anthropic.com/api/oauth/usage")
        .bearer_auth(&access)
        .header("anthropic-beta", "oauth-2025-04-20")
        .send()
        .await
        .map_err(|e| format!("usage request: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("usage endpoint: HTTP {}", resp.status()));
    }
    let usage: Value = resp.json().await.map_err(|e| format!("usage parse: {e}"))?;

    const HOUR: i64 = 3_600_000;
    const DAY: i64 = 86_400_000;
    let mut metrics = Vec::new();
    push_window(&mut metrics, usage.get("five_hour"), "Session", 5 * HOUR);
    push_window(&mut metrics, usage.get("seven_day"), "Weekly", 7 * DAY);
    push_window(&mut metrics, usage.get("seven_day_sonnet"), "Sonnet weekly", 7 * DAY);
    push_window(&mut metrics, usage.get("seven_day_opus"), "Opus weekly", 7 * DAY);
    if metrics.is_empty() {
        return Err("usage response had no recognizable limit windows".into());
    }
    Ok(Snapshot::ok(ID, NAME, plan, metrics))
}

fn push_window(metrics: &mut Vec<Metric>, node: Option<&Value>, label: &str, period_ms: i64) {
    let Some(node) = node else { return };
    let Some(used) = node.get("utilization").and_then(Value::as_f64) else { return };
    let resets_at = node
        .get("resets_at")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp_millis());
    metrics.push(Metric::progress(label, used, None).with_reset(resets_at, Some(period_ms)));
}
