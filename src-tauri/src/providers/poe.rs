use super::{http, stored_api_key, Metric, Snapshot};
use serde_json::Value;

const ID: &str = "poe";
const NAME: &str = "Poe";

pub async fn snapshot() -> Snapshot {
    match fetch().await {
        Ok(s) => s,
        Err(e) => Snapshot::error(ID, NAME, e),
    }
}

async fn fetch() -> Result<Snapshot, String> {
    let Some(key) = stored_api_key("poe", &["POE_API_KEY"]) else {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Paste a Poe API key (poe.com/api/keys) in Settings (gear icon).",
        ));
    };

    let resp = http()
        .get("https://api.poe.com/usage/current_balance")
        .bearer_auth(&key)
        .send()
        .await
        .map_err(|e| format!("balance request: {e}"))?;
    if matches!(resp.status().as_u16(), 401 | 403) {
        return Err("key was rejected — paste a fresh key in Settings (gear icon)".into());
    }
    if !resp.status().is_success() {
        return Err(format!("balance endpoint: HTTP {}", resp.status()));
    }
    let doc: Value = resp.json().await.map_err(|e| format!("balance parse: {e}"))?;
    let points = doc
        .get("current_point_balance")
        .and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        .ok_or("no point balance in response")?;

    let metrics = vec![Metric::text("Balance", format!("{points:.0} points"))];
    Ok(Snapshot::ok(ID, NAME, None, metrics))
}
