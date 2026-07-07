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
    let arr: Vec<Value> = snapshots.iter().map(|s| provider_json(s, &fetched_at)).collect();
    if let Ok(mut v) = latest().lock() {
        *v = Value::Array(arr);
    }
}

fn provider_json(s: &Snapshot, fetched_at: &str) -> Value {
    let lines: Vec<Value> = s
        .metrics
        .iter()
        .map(|m| {
            if m.kind == "progress" {
                json!({
                    "type": "progress",
                    "label": m.label,
                    "used": m.used_percent,
                    "limit": 100,
                    "format": { "kind": "percent" },
                    "resetsAt": m.resets_at.map(iso8601),
                    "periodDurationMs": m.period_ms,
                    "color": Value::Null,
                })
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
    json!({
        "providerId": s.id,
        "displayName": s.name,
        "plan": s.plan,
        "lines": lines,
        "fetchedAt": fetched_at,
    })
}

fn iso8601(epoch_ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(epoch_ms)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

fn route(method: &tiny_http::Method, url: &str) -> (u16, String) {
    let path = url.split('?').next().unwrap_or(url);
    match method {
        tiny_http::Method::Options => (204, String::new()),
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
                            a.iter().find(|p| {
                                p.get("providerId").and_then(Value::as_str) == Some(id)
                            })
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
            let (status, body) = route(request.method(), request.url());
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
