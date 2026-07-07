use super::{http, stored_api_key, Metric, Snapshot};
use serde_json::Value;

const ID: &str = "venice";
const NAME: &str = "Venice";

pub async fn snapshot() -> Snapshot {
    match fetch().await {
        Ok(s) => s,
        Err(e) => Snapshot::error(ID, NAME, e),
    }
}

async fn fetch() -> Result<Snapshot, String> {
    let Some(key) = stored_api_key("venice", &["VENICE_API_KEY"]) else {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Paste a Venice API key in Settings (gear icon).",
        ));
    };

    let resp = http()
        .get("https://api.venice.ai/api/v1/api_keys/rate_limits")
        .bearer_auth(&key)
        .send()
        .await
        .map_err(|e| format!("rate_limits request: {e}"))?;
    if resp.status().as_u16() == 401 {
        return Err("key was rejected — paste a fresh key in Settings (gear icon)".into());
    }
    if !resp.status().is_success() {
        return Err(format!("rate_limits endpoint: HTTP {}", resp.status()));
    }
    let doc: Value = resp.json().await.map_err(|e| format!("rate_limits parse: {e}"))?;
    let data = doc.get("data").unwrap_or(&doc);

    // balances: { "DIEM": 12.3, "USD": 4.5, "VCU": 67.8 } — whichever exist.
    let mut metrics = Vec::new();
    if let Some(balances) = data.get("balances").and_then(Value::as_object) {
        for (unit, amount) in balances {
            let Some(amount) = amount.as_f64() else { continue };
            let value = if unit.eq_ignore_ascii_case("usd") {
                format!("${amount:.2}")
            } else {
                format!("{amount:.2} {unit}")
            };
            metrics.push(Metric::text(&format!("Balance ({unit})"), value));
        }
    }
    if metrics.is_empty() {
        return Err("no balances in response".into());
    }

    let plan = data
        .get("apiTier")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
        .map(|id| {
            let mut c = id.chars();
            match c.next() {
                Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
                None => id.to_string(),
            }
        });
    Ok(Snapshot::ok(ID, NAME, plan, metrics))
}
