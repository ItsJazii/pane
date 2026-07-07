use super::{http, stored_api_key, Metric, Snapshot};
use serde_json::Value;

const ID: &str = "zai";
const NAME: &str = "Z.ai";

fn find_key() -> Option<String> {
    if let Some(key) = stored_api_key("zai", &["ZAI_API_KEY", "GLM_API_KEY"]) {
        return Some(key);
    }
    // The Z.ai CLI's own key file.
    let path = dirs::home_dir()?.join(".config").join("zai").join("key.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let doc: Value = serde_json::from_str(&raw).ok()?;
    doc.get("apiKey")
        .or_else(|| doc.get("api_key"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub async fn snapshot() -> Snapshot {
    match fetch().await {
        Ok(s) => s,
        Err(e) => Snapshot::error(ID, NAME, e),
    }
}

async fn fetch() -> Result<Snapshot, String> {
    let Some(key) = find_key() else {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Paste a Z.ai API key in Settings (gear icon).",
        ));
    };

    let quota_req = http()
        .get("https://api.z.ai/api/monitor/usage/quota/limit")
        .bearer_auth(&key)
        .send();
    let plan_req = http()
        .get("https://api.z.ai/api/biz/subscription/list")
        .bearer_auth(&key)
        .send();
    let (quota_resp, plan_resp) = tokio::join!(quota_req, plan_req);

    let quota_resp = quota_resp.map_err(|e| format!("quota request: {e}"))?;
    if quota_resp.status().as_u16() == 401 {
        return Err("API key was rejected — check it in Settings".into());
    }
    if !quota_resp.status().is_success() {
        return Err(format!("quota endpoint: HTTP {}", quota_resp.status()));
    }
    let quota: Value = quota_resp.json().await.map_err(|e| format!("quota parse: {e}"))?;

    let mut metrics = Vec::new();
    collect_quota_metrics(quota.get("data").unwrap_or(&quota), &mut metrics);
    if metrics.is_empty() {
        return Err("unexpected quota response shape (endpoint is undocumented)".into());
    }
    metrics.truncate(5);

    let mut plan = None;
    if let Ok(resp) = plan_resp {
        if resp.status().is_success() {
            if let Ok(doc) = resp.json::<Value>().await {
                plan = find_plan_name(doc.get("data").unwrap_or(&doc));
            }
        }
    }

    Ok(Snapshot::ok(ID, NAME, plan, metrics))
}

/// The quota endpoint is undocumented, so we parse tolerantly: any object
/// carrying a usage/limit pair (or a percentage) becomes a meter.
fn collect_quota_metrics(node: &Value, metrics: &mut Vec<Metric>) {
    match node {
        Value::Array(items) => {
            for item in items {
                collect_quota_metrics(item, metrics);
            }
        }
        Value::Object(map) => {
            // TIME_LIMIT is the monthly web-search quota, with inverted field
            // roles vs the other entries: `currentValue` = used, `usage` = cap.
            let type_name = ["type", "name"]
                .iter()
                .find_map(|k| map.get(*k).and_then(Value::as_str));
            if type_name == Some("TIME_LIMIT") {
                let used = map.get("currentValue").and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
                let cap = map.get("usage").and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
                if cap > 0.0 {
                    let resets_at = map
                        .get("nextResetTime")
                        .and_then(Value::as_i64)
                        .filter(|ms| *ms > 0);
                    metrics.push(
                        Metric::progress(
                            "Web Searches",
                            (used / cap * 100.0).clamp(0.0, 100.0),
                            Some(format!("{used:.0} of {cap:.0} searches")),
                        )
                        .with_reset(resets_at, Some(30 * 86_400_000)),
                    );
                }
                return;
            }

            let label = ["type", "name", "unit", "quotaType"]
                .iter()
                .find_map(|k| map.get(*k).and_then(Value::as_str))
                .map(nice_label)
                .unwrap_or_else(|| "Quota".to_string());

            let used = ["usage", "used", "currentValue", "current"]
                .iter()
                .find_map(|k| map.get(*k).and_then(Value::as_f64));
            let limit = ["limit", "total", "maxValue", "max"]
                .iter()
                .find_map(|k| map.get(*k).and_then(Value::as_f64));
            let percent = ["percentage", "percent", "usagePercent"]
                .iter()
                .find_map(|k| map.get(*k).and_then(Value::as_f64));

            if let Some(p) = percent {
                metrics.push(Metric::progress(&label, p, None));
            } else if let (Some(u), Some(l)) = (used, limit) {
                if l > 0.0 {
                    metrics.push(Metric::progress(
                        &label,
                        u / l * 100.0,
                        Some(format!("{u:.0} of {l:.0}")),
                    ));
                }
            } else {
                for value in map.values() {
                    collect_quota_metrics(value, metrics);
                }
            }
        }
        _ => {}
    }
}

fn nice_label(raw: &str) -> String {
    let lower = raw.to_lowercase();
    if lower.contains("5h") || lower.contains("five") || lower.contains("session") {
        "Session".to_string()
    } else if lower.contains("7d") || lower.contains("week") {
        "Weekly".to_string()
    } else if lower.contains("search") {
        "Web searches".to_string()
    } else {
        raw.to_string()
    }
}

fn find_plan_name(node: &Value) -> Option<String> {
    match node {
        Value::Array(items) => items.iter().find_map(find_plan_name),
        Value::Object(map) => ["productName", "planName", "plan", "name"]
            .iter()
            .find_map(|k| map.get(*k).and_then(Value::as_str))
            .map(str::to_string)
            .or_else(|| map.values().find_map(find_plan_name)),
        _ => None,
    }
}
