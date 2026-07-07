use super::{http, Metric, Snapshot};
use serde_json::Value;
use std::path::PathBuf;

const ID: &str = "copilot";
const NAME: &str = "Copilot";

/// GitHub tokens can come from Copilot's editor config or the GitHub CLI.
fn find_token() -> Option<String> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        candidates.push(PathBuf::from(&local).join("github-copilot").join("apps.json"));
        candidates.push(PathBuf::from(&local).join("github-copilot").join("hosts.json"));
    }
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".config").join("github-copilot").join("apps.json"));
        candidates.push(home.join(".config").join("github-copilot").join("hosts.json"));
    }
    for path in candidates {
        let Ok(raw) = std::fs::read_to_string(&path) else { continue };
        let Ok(doc) = serde_json::from_str::<Value>(&raw) else { continue };
        if let Some(map) = doc.as_object() {
            for entry in map.values() {
                if let Some(tok) = entry.get("oauth_token").and_then(Value::as_str) {
                    return Some(tok.to_string());
                }
            }
        }
    }
    // GitHub CLI. Older versions kept oauth_token in hosts.yml; modern gh
    // (which the new Copilot CLI piggybacks on) stores it in Windows
    // Credential Manager under gh:github.com[:username].
    let mut usernames: Vec<String> = Vec::new();
    if let Ok(appdata) = std::env::var("APPDATA") {
        let hosts = PathBuf::from(appdata).join("GitHub CLI").join("hosts.yml");
        if let Ok(raw) = std::fs::read_to_string(&hosts) {
            let mut in_users = false;
            for line in raw.lines() {
                let trimmed = line.trim();
                if let Some(tok) = trimmed.strip_prefix("oauth_token:") {
                    let tok = tok.trim();
                    if !tok.is_empty() {
                        return Some(tok.to_string());
                    }
                }
                // Collect usernames under a "users:" block for the
                // Credential Manager lookup below.
                if trimmed == "users:" {
                    in_users = true;
                } else if in_users {
                    let indent = line.len() - line.trim_start().len();
                    if indent >= 8 && trimmed.ends_with(':') {
                        usernames.push(trimmed.trim_end_matches(':').to_string());
                    } else if indent <= 4 && !trimmed.is_empty() {
                        in_users = false;
                    }
                }
            }
        }
    }
    let mut targets: Vec<String> =
        usernames.iter().map(|u| format!("gh:github.com:{u}")).collect();
    targets.push("gh:github.com:".into());
    targets.push("gh:github.com".into());
    for target in targets {
        if let Some(token) = super::credential_string(&target) {
            let token = token.trim().to_string();
            if !token.is_empty() {
                return Some(token);
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
    let Some(token) = find_token() else {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "No GitHub sign-in found (Copilot in an editor, or `gh auth login`).",
        ));
    };

    let resp = http()
        .get("https://api.github.com/copilot_internal/user")
        .header("Authorization", format!("token {token}"))
        .header("Accept", "application/json")
        .header("Editor-Version", "vscode/1.101.0")
        .header("Editor-Plugin-Version", "copilot-chat/0.27.0")
        .header("X-GitHub-Api-Version", "2025-04-01")
        .send()
        .await
        .map_err(|e| format!("usage request: {e}"))?;
    if resp.status().as_u16() == 401 || resp.status().as_u16() == 403 {
        return Err("GitHub token was rejected — sign in to Copilot again".into());
    }
    if !resp.status().is_success() {
        return Err(format!("usage endpoint: HTTP {}", resp.status()));
    }
    let user: Value = resp.json().await.map_err(|e| format!("usage parse: {e}"))?;

    let plan = user
        .get("copilot_plan")
        .and_then(Value::as_str)
        .map(str::to_string);
    // Monthly quotas with a known reset date, e.g. "2026-08-01".
    let resets_at = user
        .get("quota_reset_date")
        .and_then(Value::as_str)
        .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc().timestamp_millis());

    let mut metrics = Vec::new();
    if let Some(snapshots) = user.get("quota_snapshots") {
        push_quota(&mut metrics, snapshots.get("premium_interactions"), "Credits", resets_at);
        push_quota(&mut metrics, snapshots.get("chat"), "Chat", resets_at);
        push_quota(&mut metrics, snapshots.get("completions"), "Completions", resets_at);
    }
    if metrics.is_empty() {
        return Err("no quota data in response (plan may not expose quotas)".into());
    }
    Ok(Snapshot::ok(ID, NAME, plan, metrics))
}

#[cfg(test)]
mod tests {
    /// Live probe with this machine's real GitHub login — run manually via
    /// `cargo test --lib copilot -- --ignored --nocapture`. Prints statuses
    /// and counts only, never token values.
    #[test]
    #[ignore]
    fn live_probe() {
        let snap = tauri::async_runtime::block_on(super::snapshot());
        eprintln!(
            "copilot: status={} plan={:?} error={:?} metrics={}",
            snap.status,
            snap.plan,
            snap.error,
            snap.metrics.len()
        );
        for m in &snap.metrics {
            eprintln!("  {}: used={:?} value={:?}", m.label, m.used_percent, m.value);
        }
    }
}

fn push_quota(metrics: &mut Vec<Metric>, node: Option<&Value>, label: &str, resets_at: Option<i64>) {
    const MONTH_MS: i64 = 30 * 86_400_000;
    let Some(node) = node else { return };
    if node.get("unlimited").and_then(Value::as_bool) == Some(true) {
        metrics.push(Metric::text(label, "Unlimited".into()));
        return;
    }
    let Some(percent_remaining) = node.get("percent_remaining").and_then(Value::as_f64) else {
        return;
    };
    let detail = (|| {
        let remaining = node.get("remaining").and_then(Value::as_f64)?;
        let entitlement = node.get("entitlement").and_then(Value::as_f64)?;
        Some(format!("{remaining:.0} of {entitlement:.0} left"))
    })();
    metrics.push(
        Metric::progress(label, 100.0 - percent_remaining, detail)
            .with_reset(resets_at, Some(MONTH_MS)),
    );
}
