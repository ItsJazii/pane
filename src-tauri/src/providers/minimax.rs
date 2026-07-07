//! MiniMax Coding/Token Plan (M2.7 / M3 models). Quota comes from the same
//! endpoint the official `mmx quota` CLI command uses, with a Bearer API key.
//! Key sources: our Settings pane, MINIMAX_API_KEY, or the MiniMax Agent
//! CLI's ~/.minimax/config.yaml (provider.minimax.options.apiKey).

use serde_json::Value;

use super::{Metric, Snapshot};

const ID: &str = "minimax";
const NAME: &str = "MiniMax";

// Primary is the official Token Plan endpoint; the openplatform path is the
// legacy alias several trackers still use; .minimaxi.com is the CN region.
const ENDPOINTS: [&str; 4] = [
    "https://api.minimax.io/v1/token_plan/remains",
    "https://api.minimax.io/v1/api/openplatform/coding_plan/remains",
    "https://api.minimaxi.com/v1/token_plan/remains",
    "https://api.minimaxi.com/v1/api/openplatform/coding_plan/remains",
];

fn find_api_key() -> Option<String> {
    if let Some(key) = super::stored_api_key(ID, &["MINIMAX_API_KEY"]) {
        return Some(key);
    }
    // MiniMax Agent CLI config: provider.minimax.options.apiKey. A two-space
    // YAML file we only need one scalar out of, so a line scan is enough.
    let path = dirs::home_dir()?.join(".minimax").join("config.yaml");
    let raw = std::fs::read_to_string(path).ok()?;
    for line in raw.lines() {
        if let Some(v) = line.trim().strip_prefix("apiKey:") {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            // Real MiniMax keys are long; fresh CLI installs carry a short
            // "sk-…" placeholder that would only produce a confusing error.
            if v.len() > 20 {
                return Some(v.to_string());
            }
        }
    }
    None
}

pub async fn snapshot() -> Snapshot {
    match fetch().await {
        Ok(s) => s,
        Err(e) => Snapshot::error(ID, NAME, e),
    }
}

async fn fetch() -> Result<Snapshot, String> {
    let Some(key) = find_api_key() else {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "No MiniMax key found (Settings, MINIMAX_API_KEY, or the MiniMax CLI).",
        ));
    };

    let mut last_error = String::from("quota endpoint unreachable");
    for endpoint in ENDPOINTS {
        let resp = match super::http()
            .get(endpoint)
            .header("Authorization", format!("Bearer {key}"))
            .header("Content-Type", "application/json")
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last_error = format!("quota request: {e}");
                continue;
            }
        };
        if !resp.status().is_success() {
            last_error = format!("quota endpoint: HTTP {}", resp.status());
            continue;
        }
        let doc: Value = match resp.json().await {
            Ok(d) => d,
            Err(e) => {
                last_error = format!("quota parse: {e}");
                continue;
            }
        };
        // MiniMax signals auth/path problems in-band: status_code != 0.
        let status_code = doc.pointer("/base_resp/status_code").and_then(Value::as_i64);
        if status_code != Some(0) {
            let msg = doc
                .pointer("/base_resp/status_msg")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            last_error = format!("MiniMax: {msg} (code {})", status_code.unwrap_or(-1));
            continue;
        }
        if let Some(snap) = parse_remains(&doc) {
            return Ok(snap);
        }
        last_error = "no recognizable quota rows in response".into();
    }
    Err(last_error)
}

/// Picks the coding-model row: "MiniMax-M*" preferred, then "general",
/// then the largest quota row.
fn pick_row(rows: &[Value]) -> Option<&Value> {
    let named = |pred: &dyn Fn(&str) -> bool| {
        rows.iter().find(|r| {
            r.get("model_name").and_then(Value::as_str).map(pred).unwrap_or(false)
        })
    };
    named(&|n: &str| n.starts_with("MiniMax-M"))
        .or_else(|| named(&|n: &str| n == "general"))
        .or_else(|| {
            rows.iter().max_by_key(|r| {
                r.get("current_interval_total_count").and_then(Value::as_i64).unwrap_or(0)
            })
        })
}

fn parse_remains(doc: &Value) -> Option<Snapshot> {
    let rows = doc.get("model_remains").and_then(Value::as_array)?;
    let row = pick_row(rows)?;
    let num = |key: &str| row.get(key).and_then(Value::as_f64);

    let mut metrics = Vec::new();

    // 5-hour rolling window. Field-name trap (confirmed against the official
    // CLI): *_usage_count actually holds the REMAINING count.
    {
        let total = num("current_interval_total_count").unwrap_or(0.0);
        let remaining_count = num("current_interval_usage_count");
        let used_percent = num("current_interval_remaining_percent")
            .map(|p| 100.0 - p)
            .or_else(|| {
                remaining_count
                    .filter(|_| total > 0.0)
                    .map(|rem| 100.0 * (1.0 - rem / total))
            });
        if let Some(used) = used_percent {
            let detail = remaining_count
                .filter(|_| total > 0.0)
                .map(|rem| format!("{rem:.0} of {total:.0} left"));
            let resets_at = num("end_time").map(|v| v as i64).filter(|v| *v > 0);
            metrics.push(
                Metric::progress("Session", used.clamp(0.0, 100.0), detail)
                    .with_reset(resets_at, Some(5 * 60 * 60 * 1000)),
            );
        }
    }

    // Weekly window. status 3 = unlimited; boost_permille can lift the
    // remaining percent above 100 (displayed capped at 100 here).
    {
        let status = num("current_weekly_status").unwrap_or(1.0) as i64;
        if status == 3 {
            metrics.push(Metric::text("Weekly", "Unlimited".into()));
        } else if let Some(remaining) = num("current_weekly_remaining_percent") {
            let boost = num("weekly_boost_permille").unwrap_or(1000.0) / 1000.0;
            let used = (100.0 - remaining * boost).clamp(0.0, 100.0);
            let total = num("current_weekly_total_count").unwrap_or(0.0);
            let detail = num("current_weekly_usage_count")
                .filter(|_| total > 0.0)
                .map(|rem| format!("{rem:.0} of {total:.0} left"));
            let resets_at = num("weekly_end_time").map(|v| v as i64).filter(|v| *v > 0);
            metrics.push(
                Metric::progress("Weekly", used, detail)
                    .with_reset(resets_at, Some(7 * 24 * 60 * 60 * 1000)),
            );
        }
    }

    if metrics.is_empty() {
        return None;
    }
    Some(Snapshot::ok(ID, NAME, Some("Coding Plan".into()), metrics))
}

#[cfg(test)]
mod tests {
    /// Live probe with this machine's real key — run manually via
    /// `cargo test --lib minimax -- --ignored --nocapture`. Prints statuses
    /// and numbers only, never the key.
    #[test]
    #[ignore]
    fn live_probe() {
        let snap = tauri::async_runtime::block_on(super::snapshot());
        eprintln!(
            "minimax: status={} plan={:?} error={:?} metrics={}",
            snap.status,
            snap.plan,
            snap.error,
            snap.metrics.len()
        );
        for m in &snap.metrics {
            eprintln!(
                "  {}: used={:?} detail={:?} value={:?} resets_at={:?}",
                m.label, m.used_percent, m.detail, m.value, m.resets_at
            );
        }
    }
}
