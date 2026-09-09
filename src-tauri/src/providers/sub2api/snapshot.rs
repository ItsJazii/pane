use super::super::{http_no_redirect, Metric, Snapshot};
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

pub struct KeyCard {
    pub id: String,
    pub name: String,
    pub origin: String,
    pub api_key: String,
}

pub fn key_cards_at(path: &Path) -> Result<Vec<KeyCard>, String> {
    Ok(super::store::load(path)?
        .sites
        .into_iter()
        .flat_map(|site| {
            site.keys.into_iter().map(move |key| KeyCard {
                id: format!("sub2api@{}", key.id),
                name: format!("{} · {}", site.name, key.label),
                origin: site.base_url.clone(),
                api_key: key.api_key,
            })
        })
        .collect())
}

pub fn refresh_clients(cards: &[KeyCard]) -> HashMap<String, reqwest::Client> {
    let mut clients = HashMap::new();
    for card in cards {
        clients
            .entry(card.origin.clone())
            .or_insert_with(http_no_redirect);
    }
    clients
}

pub async fn snapshot_key_with_client(client: reqwest::Client, card: KeyCard) -> Snapshot {
    let mut snapshot = match fetch(&client, &card).await {
        Ok((plan, metrics)) => Snapshot::ok(&card.id, &card.name, plan, metrics),
        Err(error) => Snapshot::error(&card.id, &card.name, error),
    };
    snapshot.dashboard_url = Some(card.origin);
    snapshot
}

async fn fetch(
    client: &reqwest::Client,
    card: &KeyCard,
) -> Result<(Option<String>, Vec<Metric>), String> {
    static SEMAPHORE: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    let _permit = SEMAPHORE
        .get_or_init(|| tokio::sync::Semaphore::new(8))
        .acquire()
        .await
        .map_err(|_| "usage transport failed".to_string())?;
    let origin = super::normalize_site_url(&card.origin)?;
    let mut response = client
        .get(format!("{origin}/v1/usage"))
        .bearer_auth(&card.api_key)
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                "usage request timed out"
            } else {
                "usage transport failed"
            }
            .to_string()
        })?;
    if !response.status().is_success() {
        return Err(match response.status().as_u16() {
            401 => "usage HTTP 401: authentication failed".into(),
            403 => "usage HTTP 403: access denied".into(),
            429 => "usage HTTP 429: rate limited".into(),
            code @ 300..=399 => format!("usage HTTP {code}: redirect refused"),
            code => format!("usage HTTP {code}"),
        });
    }
    const MAX_BYTES: usize = 64 * 1024;
    if response
        .content_length()
        .is_some_and(|size| size > MAX_BYTES as u64)
    {
        return Err("usage response too large".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "usage transport failed".to_string())?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_BYTES {
            return Err("usage response too large".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let body =
        serde_json::from_slice(&bytes).map_err(|_| "usage response parse failed".to_string())?;
    parse(&body)
}

fn number(value: Option<&serde_json::Value>) -> Option<f64> {
    value?.as_f64().filter(|n| n.is_finite())
}

fn nonnegative(value: Option<&serde_json::Value>) -> Option<f64> {
    number(value).filter(|n| *n >= 0.0)
}

fn summaries(body: &serde_json::Value, metrics: &mut Vec<Metric>) {
    for (period, prefix) in [("today", "Today"), ("total", "Total")] {
        for (field, suffix) in [
            ("requests", "requests"),
            ("total_tokens", "tokens"),
            ("actual_cost", "actual cost"),
        ] {
            let value = nonnegative(
                body.get("usage")
                    .and_then(|v| v.get(period))
                    .and_then(|v| v.get(field)),
            );
            let text = value
                .map(|n| {
                    if field == "actual_cost" {
                        format!("${n:.2}")
                    } else {
                        n.to_string()
                    }
                })
                .unwrap_or_else(|| "Unknown".into());
            metrics.push(Metric::text(&format!("{prefix} {suffix}"), text));
        }
    }
}

fn date(value: Option<&serde_json::Value>) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    let parsed = chrono::DateTime::parse_from_rfc3339(value?.as_str()?).ok()?;
    // Keep dates within the ordinary four-digit calendar accepted by the UI.
    (0..=253_402_300_799_999)
        .contains(&parsed.timestamp_millis())
        .then_some(parsed)
}

fn expiry(value: Option<&serde_json::Value>, metrics: &mut Vec<Metric>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let valid = date(Some(value));
    metrics.push(Metric::text(
        "Expiry",
        valid
            .map(|d| {
                d.with_timezone(&chrono::Utc)
                    .format("%Y-%m-%d %H:%M UTC")
                    .to_string()
            })
            .unwrap_or_else(|| "Unknown".into()),
    ));
    valid.is_some_and(|d| d.timestamp_millis() <= chrono::Utc::now().timestamp_millis())
}

fn quota(label: &str, value: &serde_json::Value, metrics: &mut Vec<Metric>) -> bool {
    let used = nonnegative(value.get("used"));
    let limit = number(value.get("limit")).filter(|n| *n > 0.0);
    let remaining = number(value.get("remaining"));
    let reset = date(value.get("reset_at"));
    let amount = |n: Option<f64>| {
        n.map(|n| format!("${n:.2}"))
            .unwrap_or_else(|| "Unknown".into())
    };
    let detail = format!("{} of {}", amount(used), amount(limit));
    let mut metric = match (used, limit) {
        (Some(used), Some(limit)) => {
            // Avoid overflow for finite but extreme valid amounts.
            let ratio = (used / limit * 100.0).min(f64::MAX);
            Metric::progress(label, ratio, Some(detail))
        }
        _ => Metric::text(label, detail),
    };
    if let Some(remaining) = remaining {
        let text = format!("Remaining ${remaining:.2}");
        if metric.kind == "text" {
            metric.value = Some(format!("{} · {text}", metric.value.unwrap_or_default()));
        }
        // A separate remaining row is unnecessary when the full used/limit pair is valid.
    }
    if metric.kind == "text" {
        if let Some(reset) = reset {
            let when = reset.with_timezone(&chrono::Utc).format("%Y-%m-%d %H:%M UTC");
            metric.value = Some(format!("{} · Resets {when}", metric.value.unwrap_or_default()));
        }
    }
    metric.resets_at = reset.map(|d| d.timestamp_millis());
    // Only these windows have authoritative fixed durations. Subscription
    // allowances and total quota must not acquire an invented reset period.
    metric.period_ms = match label {
        "5h" => Some(5 * 60 * 60 * 1000),
        "1d" => Some(24 * 60 * 60 * 1000),
        "7d" => Some(7 * 24 * 60 * 60 * 1000),
        _ => None,
    };
    metrics.push(metric);
    used.zip(limit).is_some_and(|(u, l)| u >= l) || remaining.is_some_and(|r| r <= 0.0)
}

fn quota_metrics(body: &serde_json::Value, metrics: &mut Vec<Metric>) -> Result<(), String> {
    let mut exhausted = false;
    if let Some(total) = body.get("quota").filter(|v| v.is_object()) {
        exhausted |= quota("Total quota", total, metrics);
    }
    for window in ["5h", "1d", "7d"] {
        if let Some(value) = body
            .get("rate_limits")
            .and_then(|v| v.as_array())
            .and_then(|values| {
                values
                    .iter()
                    .find(|v| v.get("window").and_then(|v| v.as_str()) == Some(window))
            })
        {
            exhausted |= quota(window, value, metrics);
        }
    }
    if metrics.is_empty() {
        return Err("usage structure unrecognized".into());
    }
    let expired = expiry(body.get("expires_at"), metrics);
    let state = match body.get("status").and_then(|v| v.as_str()) {
        _ if expired => Some("Expired"),
        Some("expired") => Some("Expired"),
        Some("quota_exhausted") => Some("Quota exhausted"),
        Some("disabled" | "inactive") => Some("Disabled"),
        _ if exhausted => Some("Quota exhausted"),
        Some("active") | None => None,
        _ => Some("Unknown"),
    };
    if let Some(state) = state {
        metrics.push(Metric::text("Status", state.into()));
    }
    Ok(())
}

fn subscription_metrics(
    body: &serde_json::Value,
    sub: &serde_json::Value,
    metrics: &mut Vec<Metric>,
) {
    for (field, label) in [
        ("daily", "Daily"),
        ("weekly", "Weekly"),
        ("monthly", "Monthly"),
    ] {
        let limit = sub.get(format!("{field}_limit_usd"));
        let used = sub.get(format!("{field}_usage_usd"));
        // null and nonpositive limits explicitly mean this cycle is unconfigured.
        if limit.is_some_and(|v| v.is_null() || number(Some(v)).is_some_and(|n| n <= 0.0)) {
            continue;
        }
        if limit.is_none() && used.is_none() {
            continue;
        }
        quota(
            label,
            &serde_json::json!({"used":used,"limit":limit}),
            metrics,
        );
    }
    if number(body.get("remaining")) == Some(-1.0) {
        metrics.push(Metric::text("Subscription", "Unlimited".into()));
    } else if metrics.is_empty() {
        metrics.push(Metric::text("Subscription", "Unknown".into()));
    }
    let expired = expiry(sub.get("expires_at"), metrics);
    if expired {
        metrics.push(Metric::text("Status", "Expired".into()));
    } else if metrics
        .iter()
        .any(|m| m.used_percent.is_some_and(|p| p >= 100.0))
    {
        metrics.push(Metric::text("Status", "Quota exhausted".into()));
    }
}

fn parse(body: &serde_json::Value) -> Result<(Option<String>, Vec<Metric>), String> {
    if let Some(error) = response_error(body) {
        return Err(error.into());
    }
    let mut metrics = Vec::new();
    let mode = body.get("mode").and_then(|v| v.as_str());
    let legacy = body.get("mode").is_none();
    let subscription = body.get("subscription").filter(|v| {
        v.is_object()
            && [
                "daily_limit_usd",
                "weekly_limit_usd",
                "monthly_limit_usd",
                "daily_usage_usd",
                "weekly_usage_usd",
                "monthly_usage_usd",
                "expires_at",
            ]
            .iter()
            .any(|field| v.get(*field).is_some())
    });
    let balance = number(body.get("balance"));
    let legacy_quota = body.get("quota").is_some_and(|v| {
        v.is_object()
            && ["used", "limit", "remaining"]
                .iter()
                .any(|f| v.get(*f).is_some())
    }) || body
        .get("rate_limits")
        .and_then(|v| v.as_array())
        .is_some_and(|values| {
            values.iter().any(|v| {
                matches!(
                    v.get("window").and_then(|v| v.as_str()),
                    Some("5h" | "1d" | "7d")
                )
            })
        });
    let conflict = legacy && subscription.is_some() && balance.is_some();
    let plan = if mode == Some("quota_limited") || (legacy && legacy_quota) {
        quota_metrics(body, &mut metrics)?;
        "Key quota".to_string()
    } else if (mode == Some("unrestricted") || legacy) && subscription.is_some() && !conflict {
        subscription_metrics(body, subscription.unwrap(), &mut metrics);
        body.get("planName")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("Subscription")
            .to_string()
    } else if (mode == Some("unrestricted") || legacy) && balance.is_some() && !conflict {
        let balance = balance.unwrap();
        let unit = body.get("unit").and_then(|v| v.as_str()).unwrap_or("USD");
        let amount = if unit == "USD" {
            format!("${balance:.2}")
        } else {
            format!("{balance:.2} {unit}")
        };
        metrics.push(Metric::text("Balance", amount));
        if balance < 0.0 {
            metrics.push(Metric::text("Status", "Overdue".into()));
        }
        "Wallet".to_string()
    } else if body.get("balance").is_none() || balance.is_some() {
        let remaining = number(body.get("remaining"))
            .or(balance)
            .ok_or_else(|| "usage structure unrecognized".to_string())?;
        let amount = match body.get("unit").and_then(|v| v.as_str()) {
            Some("USD") => format!("${remaining:.2}"),
            Some(unit) => format!("{remaining:.2} {unit}"),
            None => format!("{remaining:.2}"),
        };
        metrics.push(Metric::text("Remaining amount", amount));
        metrics.push(Metric::text("Type", "Unknown".into()));
        "Unknown type".to_string()
    } else {
        return Err("usage structure unrecognized".into());
    };
    apply_status(body, &mut metrics);
    summaries(body, &mut metrics);
    Ok((Some(plan.into()), metrics))
}

fn apply_status(body: &serde_json::Value, metrics: &mut Vec<Metric>) {
    let existing = |state: &str| {
        metrics
            .iter()
            .any(|m| m.label == "Status" && m.value.as_deref() == Some(state))
    };
    let debt = existing("Overdue");
    let expired = existing("Expired")
        || date(body.get("expires_at"))
            .is_some_and(|d| d.timestamp_millis() <= chrono::Utc::now().timestamp_millis());
    let exhausted = existing("Quota exhausted");
    let status = body.get("status").and_then(|v| v.as_str());
    let state = match status {
        _ if expired => Some("Expired"),
        Some("expired") => Some("Expired"),
        Some("disabled" | "inactive") => Some("Disabled"),
        Some("quota_exhausted") => Some("Quota exhausted"),
        _ if exhausted => Some("Quota exhausted"),
        None | Some("active") => None,
        _ => Some("Unknown"),
    };
    metrics.retain(|m| m.label != "Status");
    if !metrics.iter().any(|m| m.label == "Expiry") {
        expiry(body.get("expires_at"), metrics);
    }
    let value = match (state, debt) {
        (Some(state), true) => Some(format!("{state} · Overdue")),
        (Some(state), false) => Some(state.into()),
        (None, true) => Some("Overdue".into()),
        (None, false) => None,
    };
    if let Some(value) = value {
        metrics.push(Metric::text("Status", value));
    }
}

fn response_error(body: &serde_json::Value) -> Option<&'static str> {
    let code = body
        .get("error")
        .and_then(|e| e.get("type").or_else(|| e.get("code")))
        .or_else(|| body.get("code"));
    let name = code
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        name.as_str(),
        "authentication_error" | "invalid_api_key" | "unauthorized" | "401"
    ) || code.and_then(|v| v.as_u64()) == Some(401)
    {
        Some("usage authentication failed")
    } else if matches!(
        name.as_str(),
        "permission_error" | "permission_denied" | "forbidden" | "403"
    ) || code.and_then(|v| v.as_u64()) == Some(403)
    {
        Some("usage access denied")
    } else if matches!(
        name.as_str(),
        "rate_limit_error" | "rate_limit_exceeded" | "429"
    ) || code.and_then(|v| v.as_u64()) == Some(429)
    {
        Some("usage rate limited")
    } else if body.get("error").is_some_and(|v| !v.is_null())
        || matches!(
            name.as_str(),
            "api_error" | "internal_error" | "server_error"
        )
        || code.and_then(|v| v.as_u64()).is_some_and(|n| n >= 400)
    {
        Some("usage server error")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    async fn response(body: Value) -> Snapshot {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", server.server_addr());
        let handle = std::thread::spawn(move || {
            if let Some(request) = server
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
            {
                assert_eq!(request.url(), "/v1/usage");
                assert_eq!(request.method(), &tiny_http::Method::Get);
                assert_eq!(
                    request
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv("Authorization"))
                        .unwrap()
                        .value
                        .as_str(),
                    "Bearer test-secret"
                );
                request
                    .respond(tiny_http::Response::from_string(body.to_string()))
                    .unwrap();
            }
        });
        let snapshot = snapshot_key_with_client(
            http_no_redirect(),
            KeyCard {
                id: "sub2api@test".into(),
                name: "Site · Key 1".into(),
                origin,
                api_key: "test-secret".into(),
            },
        )
        .await;
        handle.join().unwrap();
        snapshot
    }

    fn metric<'a>(snapshot: &'a Snapshot, label: &str) -> &'a Metric {
        snapshot
            .metrics
            .iter()
            .find(|m| m.label == label)
            .unwrap_or_else(|| panic!("missing {label}"))
    }

    #[tokio::test]
    async fn rolling_quota_refresh_drives_pace_alerts_before_exhaustion() {
        let cfg = json!({"notifyCuttingClose":true,"notifyWillRunOut":true,"locale":"en"});
        for (window, duration) in [("5h", 18_000_000_i64), ("1d", 86_400_000), ("7d", 604_800_000)] {
            let id = format!("sub2api@pace-{window}");
            crate::alerts::forget_snapshot(&id);
            let reset = chrono::Utc::now() + chrono::Duration::milliseconds(duration / 2);
            for (used, expected) in [(20, None), (47, Some("Cutting It Close")), (60, Some("Will Run Out")), (60, None)] {
                let mut snapshot = response(json!({"mode":"quota_limited",
                    "rate_limits":[{"window":window,"used":used,"limit":100,"reset_at":reset.to_rfc3339()}]})).await;
                snapshot.id = id.clone();
                assert_eq!(snapshot.status, "ok");
                let alerts = crate::alerts::evaluate(&[snapshot], &cfg);
                assert_eq!(alerts.first().map(|alert| alert.title.as_str()), expected, "{window} at {used}%");
                assert_eq!(alerts.len(), usize::from(expected.is_some()));
            }
            let mut next_period = response(json!({"mode":"quota_limited",
                "rate_limits":[{"window":window,"used":60,"limit":100,
                    "reset_at":(reset + chrono::Duration::milliseconds(duration)).to_rfc3339()}]})).await;
            next_period.id = id.clone();
            assert!(crate::alerts::evaluate(&[next_period], &cfg).is_empty());
            crate::alerts::forget_snapshot(&id);
        }
    }

    #[tokio::test]
    async fn wallet_refresh_preserves_zero_debt_and_actual_cost() {
        for balance in [12.5, 0.0, -2.5] {
            let snapshot = response(json!({"mode":"unrestricted", "balance":balance,
                "usage":{"today":{"requests":0,"total_tokens":42,"cost":9,"actual_cost":0.25},"total":{"cost":18}}})).await;
            assert_eq!(snapshot.status, "ok");
            assert_eq!(
                metric(&snapshot, "Balance").value,
                Some(format!("${balance:.2}"))
            );
            assert_eq!(
                metric(&snapshot, "Today requests").value.as_deref(),
                Some("0")
            );
            assert_eq!(
                metric(&snapshot, "Today actual cost").value.as_deref(),
                Some("$0.25")
            );
            assert_eq!(
                metric(&snapshot, "Total actual cost").value.as_deref(),
                Some("Unknown")
            );
            assert!(!snapshot.metrics.iter().any(|m| m.used_percent.is_some()));
            assert!(!serde_json::to_string(&snapshot)
                .unwrap()
                .contains("test-secret"));
            if balance < 0.0 {
                assert_eq!(
                    metric(&snapshot, "Status").value.as_deref(),
                    Some("Overdue")
                );
            }
        }
    }

    #[tokio::test]
    async fn quota_refresh_keeps_order_real_amounts_and_independent_expiry() {
        let snapshot = response(
            json!({"mode":"quota_limited","isValid":true,"status":"active",
            "quota":{"limit":20,"used":25,"remaining":0},"expires_at":"2020-01-01T00:00:00Z",
            "rate_limits":[{"window":"7d","used":null,"limit":70},
                {"window":"5h","used":0,"limit":5,"reset_at":"2030-01-01T05:00:00Z"},
                {"window":"1d","used":4,"limit":10,"window_start":"2030-01-01T00:00:00Z"}]}),
        )
        .await;
        assert_eq!(snapshot.status, "ok");
        assert_eq!(
            snapshot
                .metrics
                .iter()
                .take(4)
                .map(|m| m.label.as_str())
                .collect::<Vec<_>>(),
            ["Total quota", "5h", "1d", "7d"]
        );
        assert_eq!(metric(&snapshot, "Total quota").used_percent, Some(125.0));
        assert_eq!(
            metric(&snapshot, "Total quota").detail.as_deref(),
            Some("$25.00 of $20.00")
        );
        assert_eq!(metric(&snapshot, "5h").used_percent, Some(0.0));
        assert!(metric(&snapshot, "5h").resets_at.is_some());
        assert!(metric(&snapshot, "1d").resets_at.is_none());
        assert!(metric(&snapshot, "7d").used_percent.is_none());
        assert!(metric(&snapshot, "Expiry").resets_at.is_none());
        assert_eq!(
            metric(&snapshot, "Status").value.as_deref(),
            Some("Expired")
        );
        let windows = response(json!({"mode":"quota_limited","expires_at":"invalid-date", "rate_limits":[{"window":"5h","used":1,"limit":5}]})).await;
        assert_eq!(windows.status, "ok");
        assert_eq!(metric(&windows, "Expiry").value.as_deref(), Some("Unknown"));
        assert!(!windows.metrics.iter().any(|m| m.label == "Total quota"));
    }

    #[tokio::test]
    async fn partial_quota_keeps_reset_in_visible_text() {
        let snapshot = response(json!({"mode":"quota_limited","rate_limits":[
            {"window":"5h","used":null,"limit":10,"reset_at":"2030-01-01T00:00:00Z"}
        ]})).await;
        let row = metric(&snapshot, "5h");
        assert_eq!(snapshot.status, "ok");
        assert_eq!(row.kind, "text");
        assert_eq!(row.used_percent, None);
        assert_eq!(row.value.as_deref(), Some("Unknown of $10.00 · Resets 2030-01-01 00:00 UTC"));
    }

    #[tokio::test]
    async fn partial_windows_keep_amounts_and_only_explicit_valid_resets() {
        for window in ["5h", "1d", "7d"] {
            for (field, invalid) in [("used", json!(null)), ("used", json!("bad")),
                ("limit", json!(null)), ("limit", json!(0))] {
                let mut allowance = json!({"window":window,"used":2,"limit":10,
                    "reset_at":"2030-01-01T08:00:00+08:00"});
                allowance[field] = invalid;
                // An absent field and an explicitly invalid field both remain unknown.
                for absent in [false, true] {
                    if absent { allowance.as_object_mut().unwrap().remove(field); }
                    let snapshot = response(json!({"mode":"quota_limited",
                        "expires_at":"2031-01-01T00:00:00Z","rate_limits":[allowance]})).await;
                    let row = metric(&snapshot, window);
                    assert_eq!(row.kind, "text");
                    assert_eq!(row.used_percent, None);
                    assert_eq!(row.value.as_deref(), Some(if field == "used" {
                        "Unknown of $10.00 · Resets 2030-01-01 00:00 UTC"
                    } else { "$2.00 of Unknown · Resets 2030-01-01 00:00 UTC" }));
                    assert_eq!(metric(&snapshot, "Expiry").value.as_deref(), Some("2031-01-01 00:00 UTC"));
                }
            }
            for reset in [None, Some(json!(null)), Some(json!("bad")),
                Some(json!("2030-02-30T00:00:00Z")), Some(json!("+10000-01-01T00:00:00Z"))] {
                let mut allowance = json!({"window":window,"used":null,"limit":10,
                    "window_start":"2030-01-01T00:00:00Z"});
                if let Some(reset) = reset { allowance["reset_at"] = reset; }
                let snapshot = response(json!({"mode":"quota_limited",
                    "expires_at":"2031-01-01T00:00:00Z","rate_limits":[allowance]})).await;
                let row = metric(&snapshot, window);
                assert_eq!(row.value.as_deref(), Some("Unknown of $10.00"));
                assert_eq!(row.resets_at, None);
            }
        }
        let snapshot = response(json!({"mode":"quota_limited","rate_limits":[
            {"window":"5h","used":0,"limit":10,"reset_at":"2030-01-01T00:00:00Z"}
        ]})).await;
        let row = metric(&snapshot, "5h");
        assert_eq!(row.kind, "progress");
        assert_eq!(row.used_percent, Some(0.0));
        assert_eq!(row.detail.as_deref(), Some("$0.00 of $10.00"));
        assert_eq!(row.resets_at, Some(1_893_456_000_000));
    }

    // Fixed representative shapes verified against Wei-Shaw/sub2api
    // backend/internal/handler/gateway_handler.go, Usage handlers, 2026-09-05.
    #[tokio::test]
    async fn subscription_refresh_distinguishes_missing_cycles_unlimited_and_expiry() {
        let snapshot = response(json!({"mode":"unrestricted","planName":"Pro","remaining":5,
            "subscription":{"daily_usage_usd":0,"daily_limit_usd":10,
                "weekly_usage_usd":45,"weekly_limit_usd":50,"monthly_limit_usd":null,
                "weekly_window_start":"2030-01-01T00:00:00Z","expires_at":"2020-01-01T00:00:00Z"}}))
        .await;
        assert_eq!(snapshot.status, "ok");
        assert_eq!(snapshot.plan.as_deref(), Some("Pro"));
        assert_eq!(metric(&snapshot, "Daily").used_percent, Some(0.0));
        assert_eq!(metric(&snapshot, "Weekly").used_percent, Some(90.0));
        assert!(!snapshot.metrics.iter().any(|m| m.label == "Monthly"));
        assert!(snapshot.metrics.iter().all(|m| m.resets_at.is_none()));
        assert_eq!(
            metric(&snapshot, "Status").value.as_deref(),
            Some("Expired")
        );
        let unlimited = response(json!({"mode":"unrestricted","remaining":-1,"subscription":{
            "daily_limit_usd":null,"weekly_limit_usd":null,"monthly_limit_usd":null,
            "daily_usage_usd":0,"weekly_usage_usd":0,"monthly_usage_usd":0}}))
        .await;
        assert_eq!(
            metric(&unlimited, "Subscription").value.as_deref(),
            Some("Unlimited")
        );
        assert!(!unlimited
            .metrics
            .iter()
            .any(|m| m.label == "Balance" || m.used_percent.is_some()));
    }

    #[tokio::test]
    async fn legacy_and_unknown_modes_keep_amounts_without_guessing_wallets() {
        for (body, plan, label) in [
            (
                json!({"quota":{"used":1,"limit":10},"balance":12}),
                "Key quota",
                "Total quota",
            ),
            (
                json!({"rate_limits":[{"window":"5h","used":0,"limit":3}]}),
                "Key quota",
                "5h",
            ),
            (json!({"balance":0}), "Wallet", "Balance"),
            (
                json!({"subscription":{"daily_limit_usd":5,"daily_usage_usd":2}}),
                "Subscription",
                "Daily",
            ),
            (json!({"remaining":-1}), "Unknown type", "Remaining amount"),
            (
                json!({"mode":"future","balance":12,"remaining":4}),
                "Unknown type",
                "Remaining amount",
            ),
            (
                json!({"subscription":{"daily_limit_usd":5},"balance":12,"remaining":4}),
                "Unknown type",
                "Remaining amount",
            ),
        ] {
            let snapshot = response(body).await;
            assert_eq!(snapshot.status, "ok");
            assert_eq!(snapshot.plan.as_deref(), Some(plan));
            let value = metric(&snapshot, label);
            if plan == "Unknown type" {
                assert!(!value.value.as_deref().unwrap().contains('$'));
                assert_eq!(metric(&snapshot, "Type").value.as_deref(), Some("Unknown"));
            }
        }
        for body in [
            json!({}),
            json!({"mode":"unrestricted","planName":"wallet"}),
            json!({"mode":"unrestricted","balance":"bad","remaining":30}),
            json!({"mode":"future","quota":{"used":1,"limit":10}}),
        ] {
            assert_eq!(response(body).await.status, "error");
        }
    }

    #[tokio::test]
    async fn explicit_status_and_expiry_apply_to_all_modes_without_guessing_unknown_mode() {
        for body in [
            json!({"mode":"unrestricted","balance":12,"status":"disabled"}),
            json!({"mode":"unrestricted","subscription":{"daily_limit_usd":5},"status":"disabled"}),
        ] {
            let snapshot = response(body).await;
            assert_eq!(
                metric(&snapshot, "Status").value.as_deref(),
                Some("Disabled")
            );
        }
        let expired=response(json!({"mode":"unrestricted","balance":-2,"expires_at":"2020-01-01T00:00:00Z","isValid":true})).await;
        assert_eq!(
            metric(&expired, "Status").value.as_deref(),
            Some("Expired · Overdue")
        );
        for mode in [json!("future"), serde_json::Value::Null] {
            let unknown = response(json!({"mode":mode,"balance":12})).await;
            assert_eq!(unknown.plan.as_deref(), Some("Unknown type"));
            assert_eq!(
                metric(&unknown, "Remaining amount").value.as_deref(),
                Some("12.00")
            );
            assert!(!unknown.metrics.iter().any(|m| m.label == "Balance"));
        }
    }

    async fn raw_response(
        status: u16,
        body: String,
        location: Option<String>,
        timeout: bool,
    ) -> Snapshot {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", server.server_addr());
        let handle = std::thread::spawn(move || {
            if let Some(request) = server
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
            {
                if timeout {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                let mut response = tiny_http::Response::from_string(body).with_status_code(status);
                if let Some(location) = location {
                    response = response
                        .with_header(tiny_http::Header::from_bytes("Location", location).unwrap());
                }
                let _ = request.respond(response);
            }
        });
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_millis(if timeout {
                30
            } else {
                1000
            }))
            .build()
            .unwrap();
        let snapshot = snapshot_key_with_client(
            client,
            KeyCard {
                id: "sub2api@test".into(),
                name: "Test".into(),
                origin,
                api_key: "test-secret".into(),
            },
        )
        .await;
        handle.join().unwrap();
        snapshot
    }

    #[tokio::test]
    async fn failures_are_safe_and_never_accepted_as_balances() {
        for (status, body, expected) in [
            (
                401,
                json!({"balance":10,"code":"INVALID_API_KEY","message":"test-secret"}).to_string(),
                "authentication",
            ),
            (
                403,
                json!({"error":{"type":"permission_error","message":"test-secret"}}).to_string(),
                "denied",
            ),
            (429, "test-secret".into(), "rate limited"),
            (500, "test-secret".into(), "HTTP 500"),
            (200, "<html>test-secret</html>".into(), "parse"),
            (200, "{".into(), "parse"),
            (200, "x".repeat(65537), "too large"),
            (
                200,
                json!({"code":"INVALID_API_KEY","message":"test-secret","balance":20}).to_string(),
                "authentication",
            ),
            (
                200,
                json!({"error":{"type":"authentication_error","message":"test-secret"}})
                    .to_string(),
                "authentication",
            ),
        ] {
            let snapshot = raw_response(status, body, None, false).await;
            assert_eq!(snapshot.status, "error");
            assert!(
                snapshot.error.as_ref().unwrap().contains(expected),
                "{:?}",
                snapshot.error
            );
            assert!(!serde_json::to_string(&snapshot)
                .unwrap()
                .contains("test-secret"));
        }
        assert!(raw_response(200, "{}".into(), None, true)
            .await
            .error
            .unwrap()
            .contains("timed out"));
        let target = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let snapshot = raw_response(
            302,
            "".into(),
            Some(format!("http://{}/v1/usage", target.server_addr())),
            false,
        )
        .await;
        assert!(snapshot.error.unwrap().contains("redirect refused"));
        assert!(target
            .recv_timeout(std::time::Duration::from_millis(20))
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn concurrent_keys_keep_bearer_identity_and_bound_in_flight_requests() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", server.server_addr());
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let observed_maximum = maximum.clone();
        let handle = std::thread::spawn(move || {
            let mut workers = Vec::new();
            for _ in 0..20 {
                let request = server
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                let active = active.clone();
                let maximum = maximum.clone();
                workers.push(std::thread::spawn(move || {
                    assert_eq!(request.url(), "/v1/usage");
                    let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(count, Ordering::SeqCst);
                    let key = request
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv("Authorization"))
                        .unwrap()
                        .value
                        .as_str();
                    let index: usize = key.strip_prefix("Bearer secret-").unwrap().parse().unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(25));
                    active.fetch_sub(1, Ordering::SeqCst);
                    request
                        .respond(
                            tiny_http::Response::from_string(
                                json!({"mode":"unrestricted","balance":index}).to_string(),
                            )
                            .with_status_code(if index == 7 {
                                401
                            } else {
                                200
                            }),
                        )
                        .unwrap();
                }));
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        let mut tasks = tokio::task::JoinSet::new();
        let client = http_no_redirect();
        for index in 0..20 {
            let card = KeyCard {
                id: format!("sub2api@{index}"),
                name: format!("Key {index}"),
                origin: origin.clone(),
                api_key: format!("secret-{index}"),
            };
            let client = client.clone();
            tasks.spawn(async move { (index, snapshot_key_with_client(client, card).await) });
        }
        while let Some(result) = tasks.join_next().await {
            let (index, snapshot) = result.unwrap();
            assert_eq!(snapshot.id, format!("sub2api@{index}"));
            if index == 7 {
                assert_eq!(snapshot.status, "error");
            } else {
                assert_eq!(
                    metric(&snapshot, "Balance").value,
                    Some(format!("${:.2}", index as f64))
                );
            }
        }
        handle.join().unwrap();
        assert!(observed_maximum.load(Ordering::SeqCst) <= 8);
        assert!(observed_maximum.load(Ordering::SeqCst) > 1);
    }

    #[tokio::test]
    async fn repeated_refreshes_replace_mode_rows_and_never_fill_missing_stats_from_history() {
        let bodies = [
            json!({"mode":"unrestricted","balance":12,"usage":{"today":{"requests":9}}}),
            json!({"mode":"quota_limited","quota":{"used":2,"limit":10}}),
            json!({"mode":"unrestricted","planName":"Plan","subscription":{"daily_limit_usd":5,"daily_usage_usd":1}}),
        ];
        for first in &bodies {
            let old = response(first.clone()).await;
            for next in &bodies {
                let current = response(next.clone()).await;
                assert_eq!(current.id, old.id);
                assert!(!current.stale);
                match current.plan.as_deref() {
                    Some("Wallet") => assert!(!current
                        .metrics
                        .iter()
                        .any(|m| m.label == "Daily" || m.label == "Total quota")),
                    Some("Key quota") => assert!(!current
                        .metrics
                        .iter()
                        .any(|m| m.label == "Balance" || m.label == "Daily")),
                    Some("Plan") => assert!(!current
                        .metrics
                        .iter()
                        .any(|m| m.label == "Balance" || m.label == "Total quota")),
                    _ => panic!("unexpected plan"),
                }
                if next.get("usage").is_none() {
                    assert_eq!(
                        metric(&current, "Today requests").value.as_deref(),
                        Some("Unknown")
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn separate_sites_receive_only_their_own_keys() {
        let mut cards = Vec::new();
        let mut servers = Vec::new();
        for (id, key, balance) in [
            ("first", "secret-for-first", 12),
            ("second", "secret-for-second", 34),
        ] {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let origin = format!("http://{}", server.server_addr());
            cards.push(KeyCard {
                id: format!("sub2api@{id}"),
                name: id.into(),
                origin,
                api_key: key.into(),
            });
            servers.push(std::thread::spawn(move || {
                let request = server
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap()
                    .unwrap();
                assert_eq!(request.url(), "/v1/usage");
                let authorization = request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Authorization"))
                    .unwrap()
                    .value
                    .as_str();
                assert_eq!(authorization, format!("Bearer {key}"));
                request
                    .respond(tiny_http::Response::from_string(
                        json!({"mode":"unrestricted","balance":balance}).to_string(),
                    ))
                    .unwrap();
                assert!(server
                    .recv_timeout(std::time::Duration::from_millis(20))
                    .unwrap()
                    .is_none());
            }));
        }
        let mut clients = refresh_clients(&cards);
        assert_eq!(clients.len(), 2);
        let second = cards.pop().unwrap();
        let first = cards.pop().unwrap();
        let (first, second) = tokio::join!(
            snapshot_key_with_client(clients.remove(&first.origin).unwrap(), first),
            snapshot_key_with_client(clients.remove(&second.origin).unwrap(), second)
        );
        assert_eq!(first.id, "sub2api@first");
        assert_eq!(second.id, "sub2api@second");
        assert_eq!(metric(&first, "Balance").value.as_deref(), Some("$12.00"));
        assert_eq!(metric(&second, "Balance").value.as_deref(), Some("$34.00"));
        for server in servers {
            server.join().unwrap();
        }
    }
}
