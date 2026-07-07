use super::{http, stored_api_key, Metric, Snapshot};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

const ID: &str = "openai";
const NAME: &str = "OpenAI";

pub async fn snapshot() -> Snapshot {
    match fetch().await {
        Ok(s) => s,
        Err(e) => Snapshot::error(ID, NAME, e),
    }
}

async fn fetch() -> Result<Snapshot, String> {
    // The org costs endpoint needs an *Admin* key (sk-admin-…) — a regular
    // sk-… project key gets a 401, which we translate below.
    let Some(key) = stored_api_key("openai", &["OPENAI_ADMIN_KEY"]) else {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Paste an OpenAI Admin API key (platform.openai.com → Admin keys) in Settings (gear icon).",
        ));
    };

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let start = now - 30 * 24 * 3600;
    let url = format!(
        "https://api.openai.com/v1/organization/costs?start_time={start}&bucket_width=1d&limit=31"
    );
    let resp =
        http().get(&url).bearer_auth(&key).send().await.map_err(|e| format!("costs request: {e}"))?;
    if resp.status().as_u16() == 401 {
        return Err(
            "key was rejected — org costs need an Admin key from platform.openai.com/settings/organization/admin-keys"
                .into(),
        );
    }
    if !resp.status().is_success() {
        return Err(format!("costs endpoint: HTTP {}", resp.status()));
    }
    let doc: Value = resp.json().await.map_err(|e| format!("costs parse: {e}"))?;

    let mut total = 0.0f64;
    let mut today = 0.0f64;
    for bucket in doc.get("data").and_then(Value::as_array).unwrap_or(&vec![]) {
        let amount: f64 = bucket
            .get("results")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| r.get("amount")?.get("value")?.as_f64())
                    .sum()
            })
            .unwrap_or(0.0);
        total += amount;
        let end = bucket.get("end_time").and_then(Value::as_i64).unwrap_or(0);
        if end >= now {
            today += amount;
        }
    }

    let metrics = vec![
        Metric::text("Today", format!("${today:.2}")),
        Metric::text("Last 30 days", format!("${total:.2}")),
    ];
    Ok(Snapshot::ok(ID, NAME, Some("API".into()), metrics))
}
