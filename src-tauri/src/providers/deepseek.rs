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
        return Ok(keyless_snapshot());
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
    for (i, row) in doc
        .get("balance_infos")
        .and_then(Value::as_array)
        .unwrap_or(&vec![])
        .iter()
        .enumerate()
    {
        let currency = row.get("currency").and_then(Value::as_str).unwrap_or("?");
        let total = row
            .get("total_balance")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let sign = if currency == "CNY" { "¥" } else { "$" };
        // Usage line + low-credit notifications for the primary currency,
        // metered against the highest balance seen (top-ups raise it).
        if i == 0 {
            if let Some(meter) = super::credit_meter(ID, sign, total) {
                metrics.push(meter);
            }
        }
        metrics.push(Metric::text(&format!("Balance ({currency})"), format!("{sign}{total:.2}")));
    }
    if metrics.is_empty() {
        return Err("no balance info in response".into());
    }

    let available = doc.get("is_available").and_then(Value::as_bool).unwrap_or(true);
    let plan = Some(if available { "Pay as you go" } else { "Out of credit" }.to_string());
    Ok(Snapshot::ok(ID, NAME, plan, metrics))
}

/// No platform key: a machine running the DeepSeek Harness desktop app
/// still earns an ok card — its local sessions feed the spend section —
/// otherwise the usual paste-a-key hint.
fn keyless_snapshot() -> Snapshot {
    if super::dsh::sessions_present() {
        return Snapshot::ok(ID, NAME, Some("Harness".into()), vec![]);
    }
    Snapshot::no_credentials(ID, NAME, "Paste a DeepSeek API key in Settings (gear icon).")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pane-deepseek-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// The keyless branch: Harness sessions on disk → ok "Harness" card;
    /// nothing → the paste-a-key hint. DSH_HOME isolates both cases from
    /// the real home dir.
    #[test]
    fn keyless_card_follows_harness_sessions() {
        let _g = crate::providers::dsh::tests::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("DSH_HOME").ok();
        let dir = fixture("keyless");
        std::env::set_var("DSH_HOME", &dir);

        let s = keyless_snapshot();
        assert_eq!(s.status, "no_credentials");

        let sess = dir.join("sessions").join("proj").join("session-abc");
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::write(sess.join("session.v4.jsonl.zstd"), b"x").unwrap();
        let s = keyless_snapshot();
        assert_eq!(s.status, "ok");
        assert_eq!(s.plan.as_deref(), Some("Harness"));

        match prev {
            Some(v) => std::env::set_var("DSH_HOME", v),
            None => std::env::remove_var("DSH_HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
