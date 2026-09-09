//! Local read-only HTTP API, Mac parity: GET http://127.0.0.1:6736/v1/usage
//! returns the latest snapshots in the original app's documented wire format
//! (docs/local-http-api.md), so scripts written for the Mac app work here too.

use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::providers::Snapshot;

static LATEST: OnceLock<Mutex<Value>> = OnceLock::new();

fn latest() -> &'static Mutex<Value> {
    LATEST.get_or_init(|| Mutex::new(Value::Array(vec![])))
}

/// Called after each usage fetch with the enabled providers' snapshots.
pub fn publish(snapshots: &[Snapshot]) {
    let fetched_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let arr: Vec<Value> = snapshots
        .iter()
        .map(|s| provider_json(s, &fetched_at))
        .collect();
    if let Ok(mut v) = latest().lock() {
        *v = Value::Array(arr);
    }
}

pub(crate) fn publish_restored_sub2api(snapshots: &[Snapshot]) {
    let fetched_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    if let Ok(mut published) = latest().lock() {
        if let Some(values) = published.as_array_mut() {
            values.retain(|value| !value["providerId"].as_str().is_some_and(|id| id.starts_with("sub2api@")));
            values.extend(snapshots.iter().filter(|snapshot| snapshot.id.starts_with("sub2api@"))
                .map(|snapshot| provider_json(snapshot, &fetched_at)));
        }
    }
}

/// Keep the already-published view in sync with successful local mutations.
pub(crate) fn forget_snapshots(ids: &[String]) {
    retain_published(|id| !ids.iter().any(|removed| removed == id));
}

pub(crate) fn forget_disabled_snapshots(disabled: &[String]) {
    retain_published(|id| !crate::card_is_disabled(id, disabled));
}

fn retain_published(keep: impl Fn(&str) -> bool) {
    if let Ok(mut published) = latest().lock() {
        if let Some(snapshots) = published.as_array_mut() {
            snapshots.retain(|snapshot| snapshot["providerId"].as_str().is_some_and(&keep));
        }
    }
}

pub(crate) fn rename_snapshots(names: &std::collections::HashMap<String, String>) {
    if let Ok(mut published) = latest().lock() {
        if let Some(snapshots) = published.as_array_mut() {
            for snapshot in snapshots {
                if let Some(name) = snapshot["providerId"].as_str().and_then(|id| names.get(id)) {
                    snapshot["displayName"] = json!(name);
                }
            }
        }
    }
}

pub(crate) fn provider_json(s: &Snapshot, fetched_at: &str) -> Value {
    let lines: Vec<Value> = s
        .metrics
        .iter()
        .map(|m| {
            if m.kind == "progress" {
                let mut line = json!({
                    "type": "progress",
                    "label": m.label,
                    "used": m.used_percent,
                    "limit": 100,
                    "format": { "kind": "percent" },
                    "resetsAt": m.resets_at.map(iso8601),
                    "periodDurationMs": m.period_ms,
                    "color": Value::Null,
                });
                if s.id.starts_with("sub2api@") {
                    line["value"] = json!(m.value);
                    line["subtitle"] = json!(m.detail);
                }
                line
            } else {
                json!({
                    "type": "text",
                    "label": m.label,
                    "value": m.value,
                    "subtitle": m.detail,
                    "resetsAt": m.resets_at.map(iso8601),
                    "color": Value::Null,
                })
            }
        })
        .collect();
    let mut output = json!({
        "providerId": s.id,
        "displayName": s.name,
        "plan": s.plan,
        "lines": lines,
        "fetchedAt": fetched_at,
    });
    if s.id.starts_with("sub2api@") {
        output["status"] = json!(s.status);
        output["stale"] = json!(s.stale);
        output["error"] = json!(s.error);
        output["warning"] = json!(s.warning);
    }
    output
}

fn iso8601(epoch_ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(epoch_ms)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

/// Only loopback spellings may appear in the Host header. A missing header
/// is allowed (HTTP/1.0 scripts); a rebound hostname is not.
fn host_ok(host: Option<&str>) -> bool {
    let Some(host) = host else { return true };
    let host = host.trim().to_ascii_lowercase();
    let bare = host.strip_suffix(":6736").unwrap_or(&host);
    matches!(bare, "127.0.0.1" | "localhost" | "[::1]")
}

fn route(method: &tiny_http::Method, url: &str) -> (u16, String) {
    let path = url.split('?').next().unwrap_or(url);
    match method {
        tiny_http::Method::Get => {
            if path == "/v1/usage" {
                let body = latest()
                    .lock()
                    .map(|v| v.to_string())
                    .unwrap_or_else(|_| "[]".into());
                (200, body)
            } else if let Some(id) = path.strip_prefix("/v1/usage/") {
                match latest().lock() {
                    Ok(v) => v
                        .as_array()
                        .and_then(|a| {
                            a.iter()
                                .find(|p| p.get("providerId").and_then(Value::as_str) == Some(id))
                        })
                        .map(|p| (200, p.to_string()))
                        .unwrap_or((404, json!({"error": "provider_not_found"}).to_string())),
                    Err(_) => (503, json!({"error": "server_busy"}).to_string()),
                }
            } else {
                (404, json!({"error": "not_found"}).to_string())
            }
        }
        _ => (405, json!({"error": "method_not_allowed"}).to_string()),
    }
}

/// Binds 127.0.0.1:6736 and serves until the app exits. If the port is
/// taken the API is silently unavailable this session (Mac parity).
pub fn start() {
    std::thread::spawn(|| {
        let server = match tiny_http::Server::http("127.0.0.1:6736") {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[pane] local API: port 6736 unavailable ({e}) — API off");
                return;
            }
        };
        eprintln!("[pane] local API: http://127.0.0.1:6736/v1/usage");
        for request in server.incoming_requests() {
            // DNS-rebinding guard: a page can point its own hostname at
            // 127.0.0.1, making this server "same-origin" in the victim's
            // browser — CORS never applies then. Legitimate local clients
            // address us by loopback names only; anything else is refused.
            let host = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("Host"))
                .map(|h| h.value.as_str().to_string());
            let (status, body) = if host_ok(host.as_deref()) {
                route(request.method(), request.url())
            } else {
                (403, json!({"error": "forbidden_host"}).to_string())
            };
            let mut response = tiny_http::Response::from_string(body).with_status_code(status);
            // Deliberately NO Access-Control-Allow-Origin header: with
            // permissive CORS, any website the user visits could silently
            // read their usage data from this port. Browsers now block
            // cross-origin reads; scripts, widgets, and curl are unaffected
            // (CORS only constrains browsers). The Mac app allows "*" and
            // discloses it — we chose the stricter default.
            for (k, v) in [("Content-Type", "application/json")] {
                if let Ok(h) = tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()) {
                    response.add_header(h);
                }
            }
            let _ = request.respond(response);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{host_ok, provider_json, publish, route};
    use crate::providers::{Metric, Snapshot};

    #[test]
    fn sub2api_public_projection_preserves_stale_and_display_amounts_only() {
        let mut snap = Snapshot::ok("sub2api@http-state", "Site · Key", None,
            vec![Metric::progress("5h", 25.0, Some("$5.00 / $20.00".into()))]);
        snap.dashboard_url = Some("https://private.example.com".into());
        snap.stale = true;
        snap.warning = Some("HTTP 401".into());
        let output = provider_json(&snap, "2026-09-05T00:00:00Z");
        assert_eq!(output["stale"], true);
        assert_eq!(output["warning"], "HTTP 401");
        assert_eq!(output["status"], "ok");
        assert_eq!(output["lines"][0]["subtitle"], "$5.00 / $20.00");
        assert!(!output.to_string().contains("private.example.com"));
        let error = Snapshot::error("sub2api@http-state", "Site · Key", "HTTP 403".into());
        assert_eq!(provider_json(&error, "now")["error"], "HTTP 403");
        assert!(provider_json(&onenewapi_snap(), "now").get("status").is_none());
    }

    fn onenewapi_snap() -> Snapshot {
        let mut snap = Snapshot::ok(
            "onenewapi@abc",
            "Site · Key 1",
            None,
            vec![Metric::progress(
                "Usage",
                48.63,
                Some("$592.18 of $1217.82".into()),
            )],
        );
        snap.dashboard_url = Some("https://panel.example.com".into());
        snap
    }

    #[test]
    fn onenewapi_json_omits_origin_dashboard_and_secrets() {
        let snap = onenewapi_snap();
        let json = provider_json(&snap, "2026-07-26T00:00:00Z");
        assert_eq!(json["providerId"], "onenewapi@abc");
        assert_eq!(json["displayName"], "Site · Key 1");
        assert!(json.get("dashboardUrl").is_none());
        assert!(json.get("dashboard_url").is_none());
        assert!(json.get("baseUrl").is_none());
        assert!(json.get("origin").is_none());
        let raw = json.to_string();
        for leak in [
            "https://panel.example.com",
            "panel.example.com",
            "dashboard",
            "sk-",
            "apiKey",
            "api_key",
        ] {
            assert!(
                !raw.to_ascii_lowercase()
                    .contains(&leak.to_ascii_lowercase()),
                "local HTTP leaked {leak}: {raw}"
            );
        }
    }

    #[test]
    fn get_by_id_uses_full_snapshot_id() {
        let _guard = route_test_lock();
        publish(&[onenewapi_snap()]);
        let (status, body) = route(&tiny_http::Method::Get, "/v1/usage/onenewapi@abc");
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["providerId"], "onenewapi@abc");
        assert_eq!(v["displayName"], "Site · Key 1");
        assert!(!body.contains("https://panel.example.com"));
        assert!(!body.to_ascii_lowercase().contains("dashboard"));

        let (missing, _) = route(&tiny_http::Method::Get, "/v1/usage/onenewapi");
        assert_eq!(missing, 404);
    }

    fn route_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap()
    }

    #[test]
    fn sub2api_routes_follow_publish_rename_disable_and_context_removal() {
        let _guard = route_test_lock();
        let make = |id: &str| Snapshot::ok(id, "Site · Key", None,
            vec![Metric::text("Balance", "$5.00".into())]);
        publish(&[make("sub2api@http-a"), make("sub2api@http-b")]);
        let (status, body) = route(&tiny_http::Method::Get, "/v1/usage");
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap().as_array().unwrap().len(), 2);
        super::rename_snapshots(&std::collections::HashMap::from([
            ("sub2api@http-a".into(), "Renamed · Key".into())]));
        assert!(route(&tiny_http::Method::Get, "/v1/usage/sub2api@http-a").1.contains("Renamed · Key"));
        super::forget_snapshots(&["sub2api@http-a".into()]);
        assert_eq!(route(&tiny_http::Method::Get, "/v1/usage/sub2api@http-a").0, 404);
        assert_eq!(route(&tiny_http::Method::Get, "/v1/usage/sub2api@http-b").0, 200);
        super::forget_disabled_snapshots(&["sub2api".into()]);
        assert_eq!(route(&tiny_http::Method::Get, "/v1/usage/sub2api@http-b").0, 404);
        assert_eq!(route(&tiny_http::Method::Get, "/v1/usage").1, "[]");
        let mut restored = make("sub2api@http-restored");
        restored.stale = true;
        super::publish_restored_sub2api(&[restored]);
        let (status, body) = route(&tiny_http::Method::Get, "/v1/usage/sub2api@http-restored");
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["stale"], true);
        super::publish_restored_sub2api(&[]);
        assert_eq!(route(&tiny_http::Method::Get, "/v1/usage/sub2api@http-restored").0, 404);
    }

    #[test]
    fn host_header_must_be_loopback() {
        // Loopback spellings, with and without the port.
        for good in [
            "127.0.0.1:6736",
            "127.0.0.1",
            "localhost:6736",
            "LOCALHOST",
            "[::1]:6736",
        ] {
            assert!(host_ok(Some(good)), "{good} should be allowed");
        }
        // Absent header (HTTP/1.0 scripts) stays allowed.
        assert!(host_ok(None));
        // A rebound hostname resolving to 127.0.0.1 is refused — this is
        // the DNS-rebinding case CORS can't catch.
        for bad in [
            "evil.example:6736",
            "evil.example",
            "127.0.0.1.evil.example:6736",
            "localhost.evil.example",
        ] {
            assert!(!host_ok(Some(bad)), "{bad} should be refused");
        }
    }
}
