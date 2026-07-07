use super::{http, Metric, Snapshot};
use serde_json::{json, Value};
use std::path::PathBuf;

const ID: &str = "devin";
const NAME: &str = "Devin";
// Mirrors the Mac app's DevinUsageClient — the server expects an IDE-shaped
// client identity and the Connect RPC protocol header.
const COMPAT_VERSION: &str = "1.108.2";

fn credentials_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(appdata) = std::env::var("APPDATA") {
        paths.push(PathBuf::from(appdata).join("devin").join("credentials.toml"));
    }
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".local").join("share").join("devin").join("credentials.toml"));
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        paths.push(PathBuf::from(local).join("devin").join("credentials.toml"));
    }
    paths
}

/// Devin sends some numbers as JSON strings ("5000000") — accept both.
fn as_num(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    v.as_f64().or_else(|| v.as_str()?.trim().parse().ok())
}

pub async fn snapshot() -> Snapshot {
    match fetch().await {
        Ok(s) => s,
        Err(e) => Snapshot::error(ID, NAME, e),
    }
}

async fn fetch() -> Result<Snapshot, String> {
    let Some(path) = credentials_paths().into_iter().find(|p| p.exists()) else {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Devin CLI sign-in not found (credentials.toml).",
        ));
    };

    let raw = std::fs::read_to_string(&path).map_err(|e| format!("read credentials.toml: {e}"))?;
    let doc: toml::Value = toml::from_str(&raw).map_err(|e| format!("parse credentials.toml: {e}"))?;
    let api_key = doc
        .get("windsurf_api_key")
        .and_then(toml::Value::as_str)
        .ok_or("credentials.toml has no windsurf_api_key")?
        .to_string();
    let server = doc
        .get("api_server_url")
        .and_then(toml::Value::as_str)
        .unwrap_or("https://server.codeium.com")
        .trim_end_matches('/')
        .to_string();

    let resp = http()
        .post(format!(
            "{server}/exa.seat_management_pb.SeatManagementService/GetUserStatus"
        ))
        .header("Content-Type", "application/json")
        .header("Connect-Protocol-Version", "1")
        .json(&json!({
            "metadata": {
                "apiKey": api_key,
                "ideName": "devin",
                "ideVersion": COMPAT_VERSION,
                "extensionName": "devin",
                "extensionVersion": COMPAT_VERSION,
                "locale": "en",
            }
        }))
        .send()
        .await
        .map_err(|e| format!("status request: {e}"))?;
    if resp.status().as_u16() == 401 || resp.status().as_u16() == 403 {
        return Err("Devin credentials were rejected — sign in with the Devin CLI again".into());
    }
    if !resp.status().is_success() {
        return Err(format!("status endpoint: HTTP {}", resp.status()));
    }
    let body: Value = resp.json().await.map_err(|e| format!("status parse: {e}"))?;

    let plan_status = body
        .pointer("/userStatus/planStatus")
        .ok_or("response has no userStatus.planStatus")?;
    let plan_info = plan_status.get("planInfo").cloned().unwrap_or(Value::Null);

    let plan = plan_info
        .get("planName")
        .and_then(Value::as_str)
        .map(str::to_string);
    let hide_daily = plan_info
        .get("hideDailyQuota")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let daily_remaining = as_num(plan_status.get("dailyQuotaRemainingPercent"));
    let weekly_remaining = as_num(plan_status.get("weeklyQuotaRemainingPercent"));
    let daily_reset = as_num(plan_status.get("dailyQuotaResetAtUnix"));
    let weekly_reset = as_num(plan_status.get("weeklyQuotaResetAtUnix"));

    const DAY: i64 = 86_400_000;
    let to_ms = |unix: Option<f64>| unix.map(|s| (s * 1000.0) as i64);

    // Devin reports percent *remaining*; the meter shows percent *used*.
    let mut metrics = Vec::new();
    if !hide_daily {
        if let Some(remaining) = daily_remaining {
            metrics.push(
                Metric::progress("Daily", (100.0 - remaining).clamp(0.0, 100.0), None)
                    .with_reset(to_ms(daily_reset), Some(DAY)),
            );
        }
    }
    match (weekly_remaining, hide_daily, daily_remaining) {
        (Some(remaining), _, _) => {
            metrics.push(
                Metric::progress("Weekly", (100.0 - remaining).clamp(0.0, 100.0), None)
                    .with_reset(to_ms(weekly_reset), Some(7 * DAY)),
            );
        }
        // No weekly quota reported: surface the hidden daily quota in the
        // Weekly row so the card stays meaningful (same as the Mac app).
        (None, true, Some(remaining)) => {
            metrics.push(
                Metric::progress("Weekly", (100.0 - remaining).clamp(0.0, 100.0), None)
                    .with_reset(to_ms(weekly_reset), Some(7 * DAY)),
            );
        }
        _ => {}
    }
    if let Some(micros) = as_num(plan_status.get("overageBalanceMicros")) {
        metrics.push(Metric::text(
            "Extra balance",
            format!("${:.2}", micros.max(0.0) / 1_000_000.0),
        ));
    }

    if metrics.is_empty() {
        return Err("no quota data in response".into());
    }
    Ok(Snapshot::ok(ID, NAME, plan, metrics))
}
