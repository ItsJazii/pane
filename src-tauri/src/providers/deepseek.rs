use super::{http, stored_api_key, Metric, Snapshot};
use serde_json::Value;

const ID: &str = "deepseek";
const NAME: &str = "DeepSeek";

pub async fn snapshot() -> Snapshot {
    match fetch().await {
        Ok(s) => s,
        Err(e) => Snapshot::error(ID, NAME, e),
    }
}

async fn fetch() -> Result<Snapshot, String> {
    let Some(key) = stored_api_key("deepseek", &["DEEPSEEK_API_KEY"]) else {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Paste a DeepSeek API key in Settings (gear icon).",
        ));
    };

    let resp = http()
        .get("https://api.deepseek.com/user/balance")
        .bearer_auth(&key)
        .send()
        .await
        .map_err(|e| format!("balance request: {e}"))?;
    if resp.status().as_u16() == 401 {
        return Err("key was rejected — paste a fresh key in Settings (gear icon)".into());
    }
    if !resp.status().is_success() {
        return Err(format!("balance endpoint: HTTP {}", resp.status()));
    }
    let doc: Value = resp.json().await.map_err(|e| format!("balance parse: {e}"))?;

    // balance_infos rows carry stringified decimals per currency (CNY/USD).
    let mut metrics = Vec::new();
    for row in doc.get("balance_infos").and_then(Value::as_array).unwrap_or(&vec![]) {
        let currency = row.get("currency").and_then(Value::as_str).unwrap_or("?");
        let total = row
            .get("total_balance")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let sign = if currency == "CNY" { "¥" } else { "$" };
        metrics.push(Metric::text(&format!("Balance ({currency})"), format!("{sign}{total:.2}")));
    }
    if metrics.is_empty() {
        return Err("no balance info in response".into());
    }

    let available = doc.get("is_available").and_then(Value::as_bool).unwrap_or(true);
    let plan = Some(if available { "Pay as you go" } else { "Out of credit" }.to_string());
    Ok(Snapshot::ok(ID, NAME, plan, metrics))
}
