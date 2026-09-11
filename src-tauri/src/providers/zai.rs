use super::{http, stored_api_key, Metric, Snapshot};
use serde_json::Value;

const ID: &str = "zai";
const NAME: &str = "Z.ai";

/// The GLM coding-plan monitoring endpoints are identical on the
/// international site and the mainland Zhipu platform (confirmed against
/// Z.ai's own usage-query plugin, which switches purely on the domain):
/// `/api/monitor/usage/quota/limit` and friends exist on both. A key only
/// belongs to ONE of the two accounts, so like the Moonshot .ai/.cn pair
/// we try each site in turn — a mainland bigmodel.cn key 401s on api.z.ai
/// (and the international site is unreachable from some networks), at
/// which point the second base answers.
const BASES: [&str; 2] = ["https://api.z.ai", "https://open.bigmodel.cn"];

fn find_key() -> Option<String> {
    if let Some(key) = stored_api_key("zai", &["ZAI_API_KEY", "GLM_API_KEY"]) {
        return Some(key);
    }
    // The Z.ai CLI's own key file.
    let path = dirs::home_dir()?
        .join(".config")
        .join("zai")
        .join("key.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let doc: Value = serde_json::from_str(&raw).ok()?;
    doc.get("apiKey")
        .or_else(|| doc.get("api_key"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub async fn snapshot() -> Snapshot {
    match fetch(&BASES).await {
        Ok(s) => s,
        Err(e) => Snapshot::error(ID, NAME, e),
    }
}

async fn fetch(bases: &[&str]) -> Result<Snapshot, String> {
    let Some(key) = find_key() else {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Paste a Z.ai API key in Settings (gear icon).",
        ));
    };
    let mut last_err = String::from("no endpoint reachable");
    for base in bases {
        match fetch_at(base, &key).await {
            Ok(snap) => return Ok(snap),
            Err(SiteFailure::WrongSite(e)) => last_err = e,
            Err(SiteFailure::SiteError(e)) => return Err(e),
        }
    }
    Err(last_err)
}

/// Why a site refused, split so the fallback only runs when the OTHER
/// site can genuinely do better:
/// - `WrongSite`: the key doesn't live on this site (401) or the site is
///   unreachable — the sibling site may still answer.
/// - `SiteError`: this site answered and is having a bad day (rate
///   limit, 5xx, shape change). The sibling site would only 401 and
///   mask the real reason, so the fallback must NOT run.
enum SiteFailure {
    WrongSite(String),
    SiteError(String),
}

async fn fetch_at(base: &str, key: &str) -> Result<Snapshot, SiteFailure> {
    let quota_req = http()
        .get(format!("{base}/api/monitor/usage/quota/limit"))
        .bearer_auth(key)
        .send();
    let plan_req = http()
        .get(format!("{base}/api/biz/subscription/list"))
        .bearer_auth(key)
        .send();
    let (quota_resp, plan_resp) = tokio::join!(quota_req, plan_req);

    // Transport-level failure often just means this site is unreachable
    // from this network — the sibling site is worth a try.
    let quota_resp =
        quota_resp.map_err(|e| SiteFailure::WrongSite(format!("quota request: {e}")))?;
    if quota_resp.status().as_u16() == 401 {
        // A key 401s on the site it doesn't belong to.
        return Err(SiteFailure::WrongSite(
            "API key was rejected — check it in Settings".into(),
        ));
    }
    if !quota_resp.status().is_success() {
        // The right site rate-limiting or erroring must surface as-is;
        // the other site would only 401 and hide it.
        return Err(SiteFailure::SiteError(format!(
            "quota endpoint: HTTP {}",
            quota_resp.status()
        )));
    }
    let quota: Value = quota_resp
        .json()
        .await
        .map_err(|e| SiteFailure::SiteError(format!("quota parse: {e}")))?;

    let mut metrics = Vec::new();
    collect_quota_metrics(quota.get("data").unwrap_or(&quota), &mut metrics);
    if metrics.is_empty() {
        return Err(SiteFailure::SiteError(
            "unexpected quota response shape (endpoint is undocumented)".into(),
        ));
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
                let used = map
                    .get("currentValue")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
                    .max(0.0);
                let cap = map
                    .get("usage")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
                    .max(0.0);
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

#[cfg(test)]
mod tests {
    use super::fetch;
    use serde_json::json;

    /// One-shot mock site: answers exactly `responses` requests (each
    /// fetch sends quota + plan concurrently, so a full round is two).
    fn serve(responses: Vec<(u16, String)>) -> String {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = format!("http://{}", server.server_addr());
        std::thread::spawn(move || {
            for (status, body) in responses {
                let request = server.recv().unwrap();
                let response = tiny_http::Response::from_string(body).with_status_code(status);
                let _ = request.respond(response);
            }
            // Park with the socket open so later assertions can check
            // that no further request arrived.
            std::thread::park();
        });
        addr
    }

    fn quota_body() -> String {
        json!({
            "data": { "limits": [
                { "type": "TOKENS_LIMIT", "percentage": 42.5 },
                {
                    "type": "TIME_LIMIT",
                    "percentage": 10.0,
                    "currentValue": 5.0,
                    "usage": 50.0
                }
            ]}
        })
        .to_string()
    }

    fn rejected() -> String {
        "key was rejected".to_string()
    }

    #[tokio::test]
    async fn mainland_key_falls_through_to_the_second_site() {
        // The international site rejects the key and the mainland
        // platform answers with the same endpoint shape.
        let zai = serve(vec![(401, rejected()), (401, rejected())]);
        let bigmodel = serve(vec![
            (200, quota_body()),
            (200, json!({"data": []}).to_string()),
        ]);
        let snap = fetch(&[zai.as_str(), bigmodel.as_str()]).await.unwrap();
        assert_eq!(snap.status, "ok");
        assert!(!snap.metrics.is_empty());
        let used: Vec<_> = snap.metrics.iter().map(|m| m.label.as_str()).collect();
        assert!(used.contains(&"Web Searches"), "{used:?}");
    }

    #[tokio::test]
    async fn international_key_succeeds_without_touching_the_second_site() {
        let zai = serve(vec![
            (200, quota_body()),
            (200, json!({"data": []}).to_string()),
        ]);
        let bigmodel_server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let bigmodel = format!("http://{}", bigmodel_server.server_addr());
        let snap = fetch(&[zai.as_str(), bigmodel.as_str()]).await.unwrap();
        assert_eq!(snap.status, "ok");
        // The first site answered, so the second must be untouched.
        assert!(
            bigmodel_server
                .recv_timeout(std::time::Duration::from_millis(150))
                .unwrap()
                .is_none(),
            "a working first site must short-circuit the fallback"
        );
    }

    #[tokio::test]
    async fn rate_limit_on_the_correct_site_is_reported_as_is() {
        // A 429 from the site the key belongs to must surface as a 429 —
        // falling through to the sibling site would only earn a 401
        // there and mask the real reason (and dodge the cooldown).
        let zai = serve(vec![(429, "slow down".into()), (429, "slow down".into())]);
        let bigmodel_server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let bigmodel = format!("http://{}", bigmodel_server.server_addr());
        let error = match fetch(&[zai.as_str(), bigmodel.as_str()]).await {
            Ok(_) => panic!("a rate-limited site must not look like success"),
            Err(error) => error,
        };
        assert!(error.contains("429"), "{error}");
        assert!(
            bigmodel_server
                .recv_timeout(std::time::Duration::from_millis(150))
                .unwrap()
                .is_none(),
            "a site-level error must not trigger the other-site fallback"
        );
    }

    #[tokio::test]
    async fn both_sites_rejecting_the_key_is_an_error() {
        let a = serve(vec![(401, rejected()), (401, rejected())]);
        let b = serve(vec![(401, rejected()), (401, rejected())]);
        let error = match fetch(&[a.as_str(), b.as_str()]).await {
            Ok(_) => panic!("both sites rejected the key yet the fetch succeeded"),
            Err(error) => error,
        };
        assert!(error.contains("rejected"), "{error}");
    }

    #[tokio::test]
    async fn unreachable_first_site_falls_through() {
        // Networks that can't reach the international site at all must
        // still get their mainland numbers. Port 1 is never listening.
        let bigmodel = serve(vec![
            (200, quota_body()),
            (200, json!({"data": []}).to_string()),
        ]);
        let snap = fetch(&["http://127.0.0.1:1", bigmodel.as_str()])
            .await
            .unwrap();
        assert_eq!(snap.status, "ok");
        assert!(!snap.metrics.is_empty());
    }
}
