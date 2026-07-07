use super::{http, stored_api_key, Metric, Snapshot};
use serde_json::Value;

const ID: &str = "crof";
const NAME: &str = "Crof";

pub async fn snapshot() -> Snapshot {
    match fetch().await {
        Ok(s) => s,
        Err(e) => Snapshot::error(ID, NAME, e),
    }
}

async fn fetch() -> Result<Snapshot, String> {
    let Some(key) = stored_api_key("crof", &["CROF_API_KEY", "CROFAI_API_KEY"]) else {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Paste a Crof API key in Settings (gear icon).",
        ));
    };

    let resp = http()
        .get("https://crof.ai/usage_api/")
        .bearer_auth(&key)
        .send()
        .await
        .map_err(|e| format!("usage request: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("usage endpoint: HTTP {}", resp.status()));
    }
    let doc: Value = resp.json().await.map_err(|e| format!("usage parse: {e}"))?;

    let credits = doc.get("credits").and_then(Value::as_f64).unwrap_or(0.0);
    let plan = doc.get("requests_plan").and_then(Value::as_f64).unwrap_or(0.0);
    let usable = doc.get("usable_requests").and_then(Value::as_f64).unwrap_or(0.0);

    let used_pct = if plan <= 0.0 {
        100.0
    } else {
        100.0 - (usable.clamp(0.0, plan) / plan * 100.0).floor()
    };

    // Daily cap resets at midnight America/Chicago. Approximate DST by month
    // (Mar–Oct ≈ CDT −5, else CST −6) — a countdown, not a wall clock.
    let resets_at = {
        use chrono::{Datelike, Duration, FixedOffset, TimeZone, Timelike, Utc};
        let offset_hours = match Utc::now().month() {
            3..=10 => -5,
            _ => -6,
        };
        let tz = FixedOffset::east_opt(offset_hours * 3600).unwrap();
        let now = Utc::now().with_timezone(&tz);
        let midnight = tz
            .with_ymd_and_hms(now.year(), now.month(), now.day(), 0, 0, 0)
            .single()
            .map(|d| d + Duration::days(1));
        midnight.map(|d| d.timestamp_millis()).filter(|_| now.hour() < 24)
    };

    let metrics = vec![
        Metric::progress("Requests", used_pct, Some(format!("{usable:.0} requests left today")))
            .with_reset(resets_at, Some(24 * 3600 * 1000)),
        Metric::text("Credits", format!("${:.2}", (credits * 100.0).floor() / 100.0)),
    ];
    Ok(Snapshot::ok(ID, NAME, None, metrics))
}
