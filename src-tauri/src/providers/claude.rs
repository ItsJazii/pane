use super::{http, Metric, Snapshot};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::path::PathBuf;

// Claude Code's public OAuth client id — the same one the CLI itself uses.
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const ID: &str = "claude";
const NAME: &str = "Claude";
const MAX_CRED_BYTES: u64 = 64 * 1024;

fn default_dir() -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".claude"))
}

/// One discovered Claude login. The account at the default config dir keeps
/// the bare "claude" id forever (upstream OpenUsage's migration-killing
/// decision: existing layouts, pins, and API consumers never move); every
/// extra account mints "claude@<hash8>" from its accountUuid.
pub struct ClaudeAccount {
    pub id: String,
    pub name: String,
    pub dir: PathBuf,
}

/// The account identity living in a config dir: (accountUuid, label).
/// Claude Code keeps `.claude.json` inside a custom CLAUDE_CONFIG_DIR but
/// as a home-level sibling (`~/.claude.json`) for the default `~/.claude`.
fn dir_identity(dir: &std::path::Path) -> Option<(String, Option<String>)> {
    let mut candidates = vec![dir.join(".claude.json")];
    if let Some(home) = dirs::home_dir() {
        if dir == home.join(".claude") {
            candidates.push(home.join(".claude.json"));
        }
    }
    candidates.into_iter().find_map(|p| cached_identity_of(&p))
}

/// Identity parses memoized by (mtime, size): ~/.claude.json carries far
/// more than the oauthAccount and grows to multiple MB on active installs,
/// and identity is consulted several times per refresh cycle (discovery in
/// the fetch, the cache stamp, and the spend scan).
fn cached_identity_of(path: &std::path::Path) -> Option<(String, Option<String>)> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::SystemTime;
    type Entry = (SystemTime, u64, Option<(String, Option<String>)>);
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Entry>>> = OnceLock::new();

    let meta = std::fs::metadata(path).ok()?;
    let (mtime, size) = (meta.modified().ok()?, meta.len());
    let cache = CACHE.get_or_init(Default::default);
    if let Some((m, s, v)) = cache.lock().unwrap().get(path) {
        if *m == mtime && *s == size {
            return v.clone();
        }
    }
    let parsed = std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|doc| identity_from(&doc));
    cache.lock().unwrap().insert(path.to_path_buf(), (mtime, size, parsed.clone()));
    parsed
}

/// (accountUuid, label) from a parsed .claude.json — org name first, email
/// as the fallback label; no uuid, no identity.
fn identity_from(doc: &Value) -> Option<(String, Option<String>)> {
    let acct = doc.get("oauthAccount")?;
    let uuid = acct.get("accountUuid").and_then(Value::as_str)?;
    let label = acct
        .get("organizationName")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| acct.get("emailAddress").and_then(Value::as_str))
        .map(str::to_string);
    Some((uuid.to_string(), label))
}

/// Extra Claude logins beyond the default config dir: dot-dirs in the home
/// folder plus dirs under ~/.config that hold a `.credentials.json` with a
/// claudeAiOauth entry (how a second account is kept via CLAUDE_CONFIG_DIR).
/// Extraction alone is not validation: a dir that can't name its account
/// never becomes a card, a dir naming an already-seen account is skipped
/// (duplicate cards stay structurally impossible), and the uuid must clear
/// scoped_id_charset before it becomes `claude@<hash8>` — the frontend
/// interpolates that id into HTML attributes.
pub fn discover_extra_accounts() -> Vec<ClaudeAccount> {
    let default = default_dir();
    let default_identity = dir_identity(&default);
    // If the default dir HAS a login but can't name its account, the
    // duplicate-card guarantee is gone (a candidate holding that same
    // account would card twice) — discover nothing rather than risk it.
    if default.join(".credentials.json").exists() && default_identity.is_none() {
        return Vec::new();
    }
    let mut seen: Vec<String> = default_identity.map(|(u, _)| u).into_iter().collect();

    let mut out = Vec::new();
    for dir in super::account_scan_roots() {
        if dir == default {
            continue;
        }
        let has_oauth = super::read_small_text(
            &dir.join(".credentials.json"),
            MAX_CRED_BYTES,
            "credentials",
        )
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .is_some_and(|doc| doc.get("claudeAiOauth").is_some());
        if !has_oauth {
            continue;
        }
        let Some((uuid, label)) = dir_identity(&dir) else { continue };
        if !scoped_id_charset(&uuid) {
            continue;
        }
        if seen.iter().any(|u| u == &uuid) {
            continue;
        }
        seen.push(uuid.clone());
        let hash8: String = uuid.chars().filter(|c| *c != '-').take(8).collect();
        let name = match label {
            Some(l) => format!("Claude — {l}"),
            None => format!("Claude @{hash8}"),
        };
        out.push(ClaudeAccount { id: format!("claude@{hash8}"), name, dir });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// The default login's account identity, for the snapshot-cache stamp: a
/// different account signing into the default dir between launches must
/// not be served the previous account's cached card.
pub fn default_identity() -> Option<String> {
    dir_identity(&default_dir()).map(|(uuid, _)| uuid)
}

pub async fn snapshot() -> Snapshot {
    snapshot_at(default_dir(), ID.to_string(), NAME.to_string()).await
}

/// Snapshot for one account's config dir — the default card and every
/// discovered extra account run the exact same flow, only the paths and
/// the card identity differ.
pub async fn snapshot_at(dir: PathBuf, id: String, name: String) -> Snapshot {
    match fetch(&dir, &id, &name).await {
        Ok(s) => s,
        Err(e) => Snapshot::error(&id, &name, e),
    }
}

async fn fetch(dir: &std::path::Path, id: &str, name: &str) -> Result<Snapshot, String> {
    let path = dir.join(".credentials.json");
    if !path.exists() {
        return Ok(Snapshot::no_credentials(
            id,
            name,
            "Claude Code sign-in not found. Run `claude` in a terminal and log in.",
        ));
    }

    let raw = super::read_small_text(&path, MAX_CRED_BYTES, "credentials")?;
    let mut doc: Value = serde_json::from_str(&raw).map_err(|e| format!("parse credentials: {e}"))?;
    let oauth = doc
        .get("claudeAiOauth")
        .cloned()
        .ok_or("credentials file has no claudeAiOauth entry")?;

    let mut access = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let refresh = oauth
        .get("refreshToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let expires_at = oauth.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
    let plan = oauth
        .get("subscriptionType")
        .and_then(Value::as_str)
        .map(str::to_string);

    // Tokens go stale; swap the refresh token for a fresh access token when needed.
    let now_ms = Utc::now().timestamp_millis();
    if access.is_empty() || expires_at <= now_ms + 60_000 {
        if refresh.is_empty() {
            return Err("token expired and no refresh token present — run `claude` and log in again".into());
        }
        // Refresh rotates the CLI's refresh token. Stage the write-back
        // BEFORE the token call: if the refreshed pair can't replace the
        // live file, the rotation would sign the CLI out from under the
        // user. Only a token whose expiry is still in the future may skip
        // the refresh; an expired one would just earn a vendor rejection.
        let staged = stage_credentials_tmp(&path);
        if staged.is_none() && (access.is_empty() || expires_at <= now_ms) {
            return Err(
                "Claude credentials cannot be updated safely — run `claude` in a terminal".into(),
            );
        }
        if let Some(staged) = staged {
            let resp = http()
                .post("https://platform.claude.com/v1/oauth/token")
                .json(&json!({
                    "grant_type": "refresh_token",
                    "refresh_token": refresh,
                    "client_id": CLIENT_ID,
                }))
                .send()
                .await
                .map_err(|e| format!("token refresh: {e}"))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                // A rejected refresh token isn't transient: another app (Claude
                // Code itself, or a second machine) rotated it and this copy is
                // dead. Only a fresh CLI sign-in mints a working pair — say so
                // instead of leaving a bare HTTP code on the card.
                if body.contains("invalid_grant") {
                    return Err(
                        "Claude sign-in was rotated by another app — run `claude` in a terminal once and Pane recovers automatically"
                            .into(),
                    );
                }
                return Err(format!("token refresh failed: HTTP {status}"));
            }
            let tok: Value = resp.json().await.map_err(|e| format!("token refresh parse: {e}"))?;
            let new_access = tok
                .get("access_token")
                .and_then(Value::as_str)
                .ok_or("refresh response missing access_token")?
                .to_string();
            let new_refresh = tok
                .get("refresh_token")
                .and_then(Value::as_str)
                .unwrap_or(&refresh)
                .to_string();
            let expires_in = tok.get("expires_in").and_then(Value::as_i64).unwrap_or(3600);

            access = new_access.clone();

            // Refresh tokens rotate on use — write the new pair back so Claude Code
            // itself stays logged in.
            if let Some(entry) = doc.get_mut("claudeAiOauth").filter(|v| v.is_object()) {
                entry["accessToken"] = Value::from(new_access);
                entry["refreshToken"] = Value::from(new_refresh);
                entry["expiresAt"] = Value::from(now_ms + expires_in * 1000);
                backup_credentials(&path);
                // The tmp was created fresh and owner-locked at staging.
                staged
                    .write(&serde_json::to_string_pretty(&doc).unwrap_or(raw))
                    .map_err(|e| format!("write refreshed credentials: {e}"))?;
                staged
                    .commit(&path)
                    .map_err(|e| format!("write refreshed credentials: {e}"))?;
            }
        }
    }

    let resp = http()
        .get("https://api.anthropic.com/api/oauth/usage")
        .bearer_auth(&access)
        .header("anthropic-beta", "oauth-2025-04-20")
        .send()
        .await
        .map_err(|e| format!("usage request: {e}"))?;
    if !resp.status().is_success() {
        // Anthropic's 429s state how long the cooldown runs (a plan change
        // can trigger a ~25-minute one); carry it so the fetch guard can
        // bench for exactly that long instead of knocking every 5 minutes.
        if resp.status().as_u16() == 429 {
            if let Some(secs) = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
            {
                return Err(format!(
                    "usage endpoint: HTTP 429 (retry_after_s={secs})"
                ));
            }
        }
        return Err(format!("usage endpoint: HTTP {}", resp.status()));
    }
    let usage: Value = resp.json().await.map_err(|e| format!("usage parse: {e}"))?;

    const HOUR: i64 = 3_600_000;
    const DAY: i64 = 86_400_000;
    let mut metrics = Vec::new();
    push_window(&mut metrics, usage.get("five_hour"), "Session", 5 * HOUR);
    push_window(&mut metrics, usage.get("seven_day"), "Weekly", 7 * DAY);
    push_window(&mut metrics, usage.get("seven_day_sonnet"), "Sonnet weekly", 7 * DAY);
    push_window(&mut metrics, usage.get("seven_day_opus"), "Opus weekly", 7 * DAY);

    // Newer per-model weeklies (Fable era) live in a `limits` array instead of
    // legacy `seven_day_<model>` keys. Add any we don't already show.
    for entry in usage.get("limits").and_then(Value::as_array).unwrap_or(&vec![]) {
        if entry.get("kind").and_then(Value::as_str) != Some("weekly_scoped") {
            continue;
        }
        let Some(name) = entry.pointer("/scope/model/display_name").and_then(Value::as_str) else {
            continue;
        };
        let Some(percent) = entry.get("percent").and_then(Value::as_f64) else { continue };
        let label = format!("{name} weekly");
        if metrics.iter().any(|m| m.label == label) {
            continue;
        }
        let resets_at = parse_reset(entry.get("resets_at"));
        metrics
            .push(Metric::progress(&label, percent, None).with_reset(resets_at, Some(7 * DAY)));
    }

    // Extra Usage: pay-as-you-go overage spend, in cents. Bounded meter when
    // a monthly cap is set, plain dollars when uncapped, absent when unused.
    if let Some(extra) = usage.get("extra_usage") {
        let enabled = extra.get("is_enabled").and_then(Value::as_bool).unwrap_or(false);
        let used_cents = extra.get("used_credits").and_then(Value::as_f64);
        if enabled {
            if let Some(used_cents) = used_cents {
                let used = (used_cents.round()) / 100.0;
                let cap = extra
                    .get("monthly_limit")
                    .and_then(Value::as_f64)
                    .map(|c| c.round() / 100.0)
                    .filter(|c| *c > 0.0);
                if let Some(cap) = cap {
                    metrics.push(Metric::progress(
                        "Extra usage",
                        (used / cap * 100.0).clamp(0.0, 100.0),
                        Some(format!("${used:.2} of ${cap:.2} limit")),
                    ));
                } else if used > 0.0 {
                    metrics.push(Metric::text("Extra usage", format!("${used:.2} spent")));
                }
            }
        }
    }

    if metrics.is_empty() {
        return Err("usage response had no recognizable limit windows".into());
    }
    Ok(Snapshot::ok(id, name, plan, metrics))
}

/// `resets_at` arrives as ISO-8601 or epoch (seconds when < 1e10, else ms).
fn parse_reset(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::String(s) => DateTime::parse_from_rfc3339(s).ok().map(|dt| dt.timestamp_millis()),
        Value::Number(n) => {
            let n = n.as_f64()?;
            Some(if n.abs() < 1e10 { (n * 1000.0) as i64 } else { n as i64 })
        }
        _ => None,
    }
}

fn push_window(metrics: &mut Vec<Metric>, node: Option<&Value>, label: &str, period_ms: i64) {
    let Some(node) = node else { return };
    let Some(used) = node.get("utilization").and_then(Value::as_f64) else { return };
    let resets_at = parse_reset(node.get("resets_at"));
    metrics.push(Metric::progress(label, used, None).with_reset(resets_at, Some(period_ms)));
}

/// The account uuid becomes `claude@<hash8>`, which the frontend
/// interpolates into HTML attributes — only [A-Za-z0-9-] is safe there.
fn scoped_id_charset(raw: &str) -> bool {
    !raw.is_empty() && raw.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn is_regular_file(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path)
        .ok()
        .is_some_and(|m| m.is_file() && !m.file_type().is_symlink())
}

/// Proof the refreshed credential pair can replace the live file BEFORE the
/// rotating token call: any leftover or planted file at the predictable tmp
/// path is removed outright (a symlink or hardlink redirect is never written
/// through), a fresh tmp is created with create_new and owner-locked, and it
/// stays staged until commit so nothing can be planted in the gap. Drop
/// removes the tmp when the refresh never lands.
struct StagedTmp(std::path::PathBuf);

impl StagedTmp {
    fn write(&self, contents: &str) -> std::io::Result<()> {
        std::fs::write(&self.0, contents)
    }
    fn commit(self, live: &std::path::Path) -> std::io::Result<()> {
        std::fs::rename(&self.0, live)?;
        std::mem::forget(self);
        Ok(())
    }
}

impl Drop for StagedTmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn stage_credentials_tmp(path: &std::path::Path) -> Option<StagedTmp> {
    if !is_regular_file(path) {
        return None;
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::symlink_metadata(&tmp).is_ok() && std::fs::remove_file(&tmp).is_err() {
        return None;
    }
    if std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp).is_err() {
        return None;
    }
    if super::onenewapi::store::restrict_owner_only(&tmp).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    Some(StagedTmp(tmp))
}

/// Keep a copy of the CLI's own file before touching it, so a bad write can
/// never cost the user their login. A planted symlink at the backup path is
/// unlinked first — never write through it.
fn backup_credentials(path: &std::path::Path) {
    let bak = path.with_extension("json.pane-bak");
    if std::fs::symlink_metadata(&bak)
        .ok()
        .is_some_and(|m| m.file_type().is_symlink())
    {
        let _ = std::fs::remove_file(&bak);
    }
    let _ = std::fs::copy(path, &bak);
}

#[cfg(test)]
mod tests {
    use super::{identity_from, scoped_id_charset};
    use serde_json::json;

    #[test]
    fn claude_identity_extraction() {
        // Org name wins the label; email is the fallback.
        let org = json!({"oauthAccount": {"accountUuid": "u-1",
            "organizationName": "Acme", "emailAddress": "a@b.c"}});
        assert_eq!(identity_from(&org), Some(("u-1".into(), Some("Acme".into()))));
        let email_only = json!({"oauthAccount": {"accountUuid": "u-2",
            "organizationName": "  ", "emailAddress": "a@b.c"}});
        assert_eq!(identity_from(&email_only), Some(("u-2".into(), Some("a@b.c".into()))));
        // No uuid → no identity → no card (a dir that can't name its
        // account never becomes one).
        assert_eq!(identity_from(&json!({"oauthAccount": {}})), None);
        assert_eq!(identity_from(&json!({})), None);
    }

    #[test]
    fn scoped_id_charset_is_html_attribute_safe() {
        assert!(scoped_id_charset("b3f1c2d4-9a8b-4c5d-8e9f-aabbccddeeff"));
        assert!(!scoped_id_charset(""));
        assert!(!scoped_id_charset("evil\"><script>"));
        assert!(!scoped_id_charset("with space"));
    }

    #[test]
    fn staging_discards_leftover_tmp_and_cleans_up() {
        use super::stage_credentials_tmp;
        let dir = std::env::temp_dir().join(format!("pane-stage-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let live = dir.join("creds.json");
        std::fs::write(&live, "{}").unwrap();

        // No live credential file → no staging.
        assert!(stage_credentials_tmp(&dir.join("missing.json")).is_none());

        // A stale leftover at the predictable tmp path is removed outright —
        // never truncated into, so unrelated data is never clobbered by a
        // credential write.
        let tmp = live.with_extension("json.tmp");
        std::fs::write(&tmp, b"unrelated data").unwrap();
        {
            let staged = stage_credentials_tmp(&live).expect("staging must succeed");
            assert_eq!(std::fs::read_to_string(&tmp).unwrap(), "");
            staged.write("{\"new\":1}").unwrap();
            staged.commit(&live).unwrap();
        }
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "{\"new\":1}");

        // A staged tmp whose refresh never lands is cleaned up on drop.
        let staged = stage_credentials_tmp(&live).unwrap();
        drop(staged);
        assert!(!tmp.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
