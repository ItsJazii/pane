use super::{http, stored_api_key, Metric, Snapshot};
use serde_json::Value;

const ID: &str = "stepfun";
const NAME: &str = "StepFun";

// One host per region, same paths — api.stepfun.com serves CN keys.
const ACCOUNT_URLS: [&str; 2] = [
    "https://api.stepfun.ai/v1/accounts",
    "https://api.stepfun.com/v1/accounts",
];
// Step Plan keys carry no wallet — the accounts endpoint rejects them.
// /models answers 200 for a valid Step Plan key, which is the only signal
// the API offers: plan quota itself is dashboard-only.
const PLAN_MODELS_URLS: [&str; 2] = [
    "https://api.stepfun.ai/step_plan/v1/models",
    "https://api.stepfun.com/step_plan/v1/models",
];

pub async fn snapshot() -> Snapshot {
    match fetch().await {
        Ok(s) => s,
        Err(e) => Snapshot::error(ID, NAME, e),
    }
}

/// GET `urls` in order with the key; a 401 falls through to the next host
/// (a key is valid on one region only). Returns the answering url with
/// the first non-401 response, or None when every host rejected the key.
async fn try_urls<'u>(
    urls: &[&'u str],
    key: &str,
) -> Result<Option<(&'u str, reqwest::Response)>, String> {
    for url in urls {
        let resp = http()
            .get(*url)
            .bearer_auth(key)
            .send()
            .await
            .map_err(|e| format!("{url}: {e}"))?;
        if resp.status().as_u16() != 401 {
            return Ok(Some((*url, resp)));
        }
    }
    Ok(None)
}

async fn fetch() -> Result<Snapshot, String> {
    let Some(key) = stored_api_key("stepfun", &["STEPFUN_API_KEY", "STEP_API_KEY"]) else {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Paste a StepFun API key in Settings (gear icon).",
        ));
    };

    match try_urls(&ACCOUNT_URLS, &key).await? {
        Some((url, resp)) => {
            if !resp.status().is_success() {
                return Err(format!("accounts endpoint: HTTP {}", resp.status()));
            }
            let doc: Value = resp.json().await.map_err(|e| format!("accounts parse: {e}"))?;
            // .com accounts are billed in CNY.
            let sign = if url.contains("stepfun.com") { "¥" } else { "$" };
            let (plan, metrics) = parse_account(&doc, sign)?;
            Ok(Snapshot::ok(ID, NAME, plan, metrics))
        }
        None => {
            // Both wallet endpoints rejected the key — it may be a Step
            // Plan key, which only exists on the plan surface. A 200 on
            // /models proves the key is real rather than a typo.
            let plan_probe = try_urls(&PLAN_MODELS_URLS, &key).await?;
            if plan_probe.is_some_and(|(_, r)| r.status().is_success()) {
                let mut snap = Snapshot::ok(
                    ID,
                    NAME,
                    Some("Step Plan".into()),
                    vec![Metric::text("Monthly Credits", "Dashboard only".into())],
                );
                snap.warning = Some(
                    "StepFun exposes Step Plan usage only on its web dashboard \
                     (platform.stepfun.ai), not through the API key — spend \
                     below comes from local CLI logs."
                        .into(),
                );
                return Ok(snap);
            }
            Err("key was rejected — paste a fresh key in Settings (gear icon)".into())
        }
    }
}

/// `GET /v1/accounts` body: `{object:"account", type:"prepaid"|"postpaid",
/// balance, total_cash_balance, total_voucher_balance}`. `sign` is the
/// region's currency ($ on .ai, ¥ on .com).
fn parse_account(doc: &Value, sign: &str) -> Result<(Option<String>, Vec<Metric>), String> {
    let balance = doc
        .get("balance")
        .and_then(Value::as_f64)
        .ok_or_else(|| "account response has no balance".to_string())?;

    let mut metrics = Vec::new();
    // Credits-used meter against the highest balance seen locally —
    // top-ups raise it (feeds the Almost Out notification).
    if let Some(meter) = super::credit_meter(ID, sign, balance) {
        metrics.push(meter);
    }
    metrics.push(Metric::text("Balance", format!("{sign}{balance:.2}")));
    let vouchers = doc
        .get("total_voucher_balance")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if vouchers > 0.0 {
        metrics.push(Metric::text("Vouchers", format!("{sign}{vouchers:.2}")));
    }

    let plan = doc.get("type").and_then(Value::as_str).and_then(|t| match t {
        "prepaid" => Some("Prepaid".to_string()),
        "postpaid" => Some("Postpaid".to_string()),
        _ => None,
    });
    Ok((plan, metrics))
}

#[cfg(test)]
mod tests {
    use super::{parse_account, Metric};
    use serde_json::json;

    fn text_row<'a>(metrics: &'a [Metric], label: &str) -> Option<&'a Metric> {
        metrics.iter().find(|m| m.kind == "text" && m.label == label)
    }

    #[test]
    fn prepaid_with_vouchers_shows_plan_balance_and_vouchers() {
        let (plan, metrics) = parse_account(&json!({
            "object": "account",
            "type": "prepaid",
            "balance": 12.345,
            "total_cash_balance": 10.0,
            "total_voucher_balance": 2.345,
        }), "$")
        .unwrap();
        assert_eq!(plan.as_deref(), Some("Prepaid"));
        assert_eq!(
            text_row(&metrics, "Balance").and_then(|m| m.value.as_deref()),
            Some("$12.35")
        );
        assert_eq!(
            text_row(&metrics, "Vouchers").and_then(|m| m.value.as_deref()),
            Some("$2.35")
        );
    }

    #[test]
    fn cn_accounts_show_yuan() {
        // .com accounts bill in CNY — same body, ¥ sign.
        let (plan, metrics) = parse_account(&json!({
            "object": "account",
            "type": "prepaid",
            "balance": 99.08,
            "total_cash_balance": 100.0,
            "total_voucher_balance": 0.0,
        }), "¥")
        .unwrap();
        assert_eq!(plan.as_deref(), Some("Prepaid"));
        assert_eq!(
            text_row(&metrics, "Balance").and_then(|m| m.value.as_deref()),
            Some("¥99.08")
        );
        assert!(text_row(&metrics, "Vouchers").is_none());
    }

    #[test]
    fn postpaid_without_vouchers_hides_the_voucher_row() {
        let (plan, metrics) = parse_account(&json!({
            "object": "account",
            "type": "postpaid",
            "balance": 4.0,
            "total_cash_balance": 4.0,
            "total_voucher_balance": 0.0,
        }), "$")
        .unwrap();
        assert_eq!(plan.as_deref(), Some("Postpaid"));
        assert_eq!(
            text_row(&metrics, "Balance").and_then(|m| m.value.as_deref()),
            Some("$4.00")
        );
        assert!(text_row(&metrics, "Vouchers").is_none());
    }

    #[test]
    fn missing_balance_is_an_error() {
        assert!(parse_account(&json!({"object": "account", "type": "prepaid"}), "$").is_err());
    }
}
