use super::{http, stored_api_key, Metric, Snapshot};
use serde_json::Value;

const ID: &str = "moonshot";
const NAME: &str = "Moonshot";

// Global platform first, mainland China second — same key shape either way.
const ENDPOINTS: [&str; 2] = [
    "https://api.moonshot.ai/v1/users/me/balance",
    "https://api.moonshot.cn/v1/users/me/balance",
];

pub async fn snapshot() -> Snapshot {
    match fetch().await {
        Ok(s) => s,
        Err(e) => Snapshot::error(ID, NAME, e),
    }
}

async fn fetch() -> Result<Snapshot, String> {
    let Some(key) = stored_api_key("moonshot", &["MOONSHOT_API_KEY", "KIMI_API_KEY"]) else {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Paste a Moonshot (Kimi) API key in Settings (gear icon).",
        ));
    };

    let mut last_err = String::from("no endpoint reachable");
    for url in ENDPOINTS {
        let resp = match http().get(url).bearer_auth(&key).send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("balance request: {e}");
                continue;
            }
        };
        if resp.status().as_u16() == 401 {
            last_err = "key was rejected — paste a fresh key in Settings (gear icon)".into();
            continue; // a .ai key 401s on .cn and vice versa — try the other
        }
        if !resp.status().is_success() {
            last_err = format!("balance endpoint: HTTP {}", resp.status());
            continue;
        }
        let doc: Value = resp.json().await.map_err(|e| format!("balance parse: {e}"))?;
        let data = doc.get("data").unwrap_or(&doc);
        let available = data.get("available_balance").and_then(Value::as_f64);
        let voucher = data.get("voucher_balance").and_then(Value::as_f64);
        let cash = data.get("cash_balance").and_then(Value::as_f64);

        let Some(available) = available else {
            last_err = "no balance in response".into();
            continue;
        };
        let sign = if url.contains(".cn") { "¥" } else { "$" };
        let mut metrics = vec![Metric::text("Balance", format!("{sign}{available:.2}"))];
        if let Some(v) = voucher {
            if v > 0.0 {
                metrics.push(Metric::text("Vouchers", format!("{sign}{v:.2}")));
            }
        }
        if let Some(c) = cash {
            metrics.push(Metric::text("Cash", format!("{sign}{c:.2}")));
        }
        return Ok(Snapshot::ok(ID, NAME, Some("Pay as you go".into()), metrics));
    }
    Err(last_err)
}
