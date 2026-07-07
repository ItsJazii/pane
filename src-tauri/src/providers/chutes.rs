use super::{http, stored_api_key, Metric, Snapshot};
use serde_json::Value;

const ID: &str = "chutes";
const NAME: &str = "Chutes";

pub async fn snapshot() -> Snapshot {
    match fetch().await {
        Ok(s) => s,
        Err(e) => Snapshot::error(ID, NAME, e),
    }
}

/// Chutes' payload shapes drift; keys are matched by normalized name family.
fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase()
}

fn find_num(obj: &Value, families: &[&str]) -> Option<f64> {
    let map = obj.as_object()?;
    for (k, v) in map {
        let n = norm(k);
        if families.iter().any(|f| n == norm(f)) {
            if let Some(x) = v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())) {
                return Some(x);
            }
        }
    }
    None
}

fn find_reset_ms(obj: &Value) -> Option<i64> {
    const KEYS: [&str; 10] = [
        "reset_at", "resets_at", "reset_time", "next_reset_at", "renews_at", "renewal_at",
        "period_end", "current_period_end", "expires_at", "window_end",
    ];
    let map = obj.as_object()?;
    for (k, v) in map {
        let n = norm(k);
        if KEYS.iter().any(|f| n == norm(f)) {
            match v {
                Value::Number(x) => {
                    let x = x.as_i64()?;
                    return Some(if x > 10_000_000_000 { x } else { x * 1000 });
                }
                Value::String(s) => {
                    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s) {
                        return Some(d.timestamp_millis());
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// A window object → (used_pct, description, reset).
fn parse_window(obj: &Value) -> Option<(f64, String, Option<i64>)> {
    let limit = find_num(obj, &["limit", "cap", "max", "maximum", "quota", "quota_limit", "monthly_cap", "monthly_limit", "request_limit", "token_limit", "hard_limit", "total"]);
    let used = find_num(obj, &["used", "usage", "used_amount", "consumed", "current", "current_usage", "requests", "request_count", "tokens", "token_usage", "monthly_usage"]);
    let remaining = find_num(obj, &["remaining", "available", "balance", "left"]);
    let pct_direct = find_num(obj, &["percent_used", "usage_percent", "used_percent", "utilization", "utilization_percent"])
        .map(|v| if v.abs() < 1.0 { v * 100.0 } else { v })
        .or_else(|| find_num(obj, &["percent_remaining"]).map(|v| 100.0 - if v.abs() < 1.0 { v * 100.0 } else { v }));

    let limit = limit.or(used.zip(remaining).map(|(u, r)| u + r));
    let pct = pct_direct.or_else(|| match (used, limit) {
        (Some(u), Some(l)) if l > 0.0 => Some(u / l * 100.0),
        _ => None,
    })?;
    let desc = match (used, limit) {
        (Some(u), Some(l)) => format!("{u:.0}/{l:.0} credits"),
        _ => format!("{pct:.0}% used"),
    };
    Some((pct.clamp(0.0, 100.0), desc, find_reset_ms(obj)))
}

async fn get(key: &str, path: &str) -> Result<Value, String> {
    let resp = http()
        .get(format!("https://api.chutes.ai{path}"))
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| format!("{path}: {e}"))?;
    if matches!(resp.status().as_u16(), 401 | 403) {
        return Err("key was rejected — paste a fresh key in Settings (gear icon)".into());
    }
    if !resp.status().is_success() {
        return Err(format!("{path}: HTTP {}", resp.status()));
    }
    resp.json().await.map_err(|e| format!("{path} parse: {e}"))
}

async fn fetch() -> Result<Snapshot, String> {
    let Some(key) = stored_api_key("chutes", &["CHUTES_API_KEY"]) else {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Paste a Chutes API key in Settings (gear icon).",
        ));
    };

    let doc = get(&key, "/users/me/subscription_usage").await?;

    let mut rolling: Option<(f64, String, Option<i64>)> = None;
    let mut monthly: Option<(f64, String, Option<i64>)> = None;
    if let Some(map) = doc.as_object() {
        for (k, v) in map {
            let n = norm(k);
            if ["rolling", "rollingwindow", "rolling4h", "fourhour", "fourhourusage", "window4h"].contains(&n.as_str()) {
                rolling = rolling.or_else(|| parse_window(v));
            } else if ["monthly", "monthlyusage", "subscription", "subscriptionusage", "billingperiod"].contains(&n.as_str()) {
                monthly = monthly.or_else(|| parse_window(v));
            }
        }
    }
    // Whole payload as a single window if nothing matched by name.
    if rolling.is_none() && monthly.is_none() {
        monthly = parse_window(&doc);
    }

    let mut metrics = Vec::new();
    if let Some((pct, desc, reset)) = rolling {
        metrics.push(
            Metric::progress("4-hour quota", pct, Some(desc)).with_reset(reset, Some(4 * 3600 * 1000)),
        );
    }
    if let Some((pct, desc, reset)) = monthly {
        metrics.push(
            Metric::progress("Monthly", pct, Some(desc)).with_reset(reset, Some(30 * 24 * 3600 * 1000)),
        );
    }
    if metrics.is_empty() {
        return Err("no quota data in response".into());
    }

    let plan = doc
        .pointer("/subscription/plan_name")
        .or_else(|| doc.pointer("/subscription/plan"))
        .or_else(|| doc.pointer("/subscription/tier"))
        .or_else(|| doc.get("plan"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(Snapshot::ok(ID, NAME, plan, metrics))
}
