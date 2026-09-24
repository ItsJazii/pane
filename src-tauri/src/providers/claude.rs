use super::{codex::RedeemOutcome, http, Metric, ResetCredit, Snapshot};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

// Claude Code's public OAuth client id — the same one the CLI itself uses.
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const ID: &str = "claude";
const NAME: &str = "Claude";
// Anthropic gates cedar_ember (banked resets) eligibility by User-Agent:
// without the CLI's UA the usage endpoint answers eligible:false
// "surface"; with it the same token reports real grants.
const CLAUDE_CLI_UA: &str = "claude-cli/2.1.280 (external, cli)";
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

/// True when `dir` is a Claude Code config dir with no OAuth login —
/// `history.jsonl` + `projects/` + `shell-snapshots/` are the shape the
/// CLI writes into a `CLAUDE_CONFIG_DIR` pointed at a third-party
/// Anthropic-compatible endpoint (StepFun, MiniMax, …). The three markers
/// together exclude `~/.cursor`, `~/.qwen`, `~/.commandcode`, which each
/// carry `projects/*.jsonl` in other formats. Dirs WITH
/// `.credentials.json` are the OAuth path's business
/// (`discover_extra_accounts`) and are excluded here so nothing is
/// scanned twice.
pub(crate) fn is_keyless_claude_dir(dir: &Path) -> bool {
    dir.join("history.jsonl").is_file()
        && dir.join("projects").is_dir()
        && dir.join("shell-snapshots").is_dir()
        && !dir.join(".credentials.json").exists()
}

/// Keyless Claude config dirs under the usual scan roots (never the
/// default dir — it is scanned regardless of login state).
pub fn discover_keyless_dirs() -> Vec<PathBuf> {
    let default = default_dir();
    let mut out: Vec<PathBuf> = super::account_scan_roots()
        .into_iter()
        .filter(|d| *d != default && is_keyless_claude_dir(d))
        .collect();
    out.sort();
    out.dedup();
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

    let (access, plan) = oauth_access(dir).await?;

    let resp = http()
        // ?cedar_ember=1 asks for the banked limit-reset block alongside
        // the windows — the program's internal name. The CLI's User-Agent
        // is required too: Anthropic gates cedar_ember eligibility on it
        // (without it, every token answers eligible:false "surface").
        .get("https://api.anthropic.com/api/oauth/usage?cedar_ember=1")
        .bearer_auth(&access)
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", CLAUDE_CLI_UA)
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
        // Server display names are arbitrary text that can reach the
        // telemetry boundary via starred metrics — map them onto the fixed
        // family vocabulary the legacy seven_day_<model> labels already use.
        let lower = name.to_ascii_lowercase();
        let family = if lower.contains("opus") {
            "Opus"
        } else if lower.contains("sonnet") {
            "Sonnet"
        } else if lower.contains("haiku") {
            "Haiku"
        } else {
            "Model"
        };
        let label = format!("{family} weekly");
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
    // Pushed after the windowless early-return: a resets row alone must
    // never turn a response with no windows into an ok snapshot.
    if let Some(resets) = banked_resets(&usage) {
        metrics.push(resets);
    }
    Ok(Snapshot::ok(id, name, plan, metrics))
}

/// Read the OAuth pair in dir's `.credentials.json`, refreshing it when
/// stale — the exact flow fetch() ran, shared with redeem_credit.
/// Refresh rotates the CLI's tokens, so the new pair is staged and
/// written back BEFORE it can be used; a pair that can't replace the
/// live file is never returned (that would sign the CLI out).
/// Returns (access token, subscriptionType).
async fn oauth_access(dir: &std::path::Path) -> Result<(String, Option<String>), String> {
    let path = dir.join(".credentials.json");
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
    Ok((access, plan))
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

/// Banked "limit resets" — Anthropic's cedar_ember program, surfaced by
/// Claude Desktop in Settings → Usage. Eligibility is the gate: an
/// ineligible block (the answer when the CLI User-Agent isn't sent)
/// yields no row at all. Only the grant the server would actually spend
/// gets a claimable credit — `id` set on exactly one copy per grant,
/// since each claim consumes one reset from it; the rest of its banked
/// resets (and every other grant's) stay display-only.
fn banked_resets(usage: &Value) -> Option<Metric> {
    let ember = usage.get("cedar_ember")?;
    if !ember.is_object() || ember.get("eligible").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let next_grant = ember.get("next_grant_id").and_then(Value::as_str);
    // An active cooldown benches claims until it passes.
    let cooling = parse_reset(ember.get("cooldown_until"))
        .is_some_and(|until| until > Utc::now().timestamp_millis());
    const MAX_CREDITS: usize = 20;
    let mut credits: Vec<ResetCredit> = Vec::new();
    let grants = ember.get("grants").and_then(Value::as_array);
    'grants: for grant in grants.map(Vec::as_slice).unwrap_or(&[]) {
        // The same drops Claude Desktop applies before offering a grant:
        // no id, fewer than one total reset, a missing resets_left, or an
        // unparseable ends_at.
        let id = grant.get("id").and_then(Value::as_str).unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        let total = grant.get("resets_total").and_then(Value::as_i64).unwrap_or(0);
        if total < 1 {
            continue;
        }
        let Some(left) = grant.get("resets_left").and_then(Value::as_i64) else {
            continue;
        };
        let Some(ends_at) = parse_reset(grant.get("ends_at")) else {
            continue;
        };
        // The redeem endpoint only accepts the server-selected grant —
        // offer Use on it alone, and only while it says it's usable.
        let claimable = Some(id) == next_grant
            && grant.get("usable_now").and_then(Value::as_bool) == Some(true)
            && grant.get("paused").and_then(Value::as_bool) != Some(true)
            && !cooling
            && valid_grant_id(id);
        let left_clamped = left.clamp(0, total);
        // The claimable copy's id carries the grant's remaining count
        // (`{grant}~{left}`): every claim spends one reset, so the
        // refreshed grant~N-1 must not look like the already-claimed
        // grant~N to the popover's claimed-set/idempotency-key map —
        // those assume each id is a one-time credit.
        let mut id_spent = false;
        for _ in 0..left_clamped {
            if credits.len() >= MAX_CREDITS {
                break 'grants;
            }
            credits.push(ResetCredit {
                id: if claimable && !id_spent {
                    id_spent = true;
                    Some(format!("{id}~{left_clamped}"))
                } else {
                    None
                },
                expires_at: Some(ends_at),
            });
        }
    }
    Some(Metric::resets(credits.len(), Some(credits)))
}

/// Grant ids go into a redeem POST body — bound the shape so a malformed
/// (or hostile) response can't smuggle arbitrary strings into the call.
fn valid_grant_id(id: &str) -> bool {
    (1..=40).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// A claimable credit id is `{grant_id}~{resets_left}` — the suffix is
/// the grant's remaining count at offer time (see banked_resets). Split
/// on the LAST '~': grant ids can't contain one (valid_grant_id rejects
/// it), so anything left of it either is the grant or fails validation.
fn parse_reset_credit_id(credit_id: &str) -> Option<&str> {
    let (grant, left) = credit_id.rsplit_once('~')?;
    if !valid_grant_id(grant) {
        return None;
    }
    // Any positive remaining count is legitimate — nothing bounds how
    // many resets a grant can bank.
    match left.parse::<u32>() {
        Ok(n) if n >= 1 => Some(grant),
        _ => None,
    }
}

/// The frontend's per-credit idempotency key, reused on retries.
fn valid_request_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The redeem endpoint answers {result, reason?, cleared?} — map it onto
/// the same outcome vocabulary the popover already shows for Codex.
fn claude_redeem_outcome(status: u16, body: &Value) -> Result<RedeemOutcome, String> {
    if status == 429 {
        return Err("rate limited — try again in a few minutes".into());
    }
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}"));
    }
    let cleared = body
        .get("cleared")
        .and_then(Value::as_array)
        .map_or(0, |c| c.len() as i64);
    match body.get("result").and_then(Value::as_str) {
        // already_used is the idempotency-key retry landing — the reset
        // was spent (by us, earlier), which for the UI is a success.
        Some("reset") | Some("already_used") => Ok(RedeemOutcome {
            outcome: "success",
            message: "Claude limits reset".into(),
            windows_reset: cleared,
        }),
        Some("not_limited") => Ok(RedeemOutcome {
            outcome: "nothing_to_reset",
            message: "Your usage doesn't need a reset yet".into(),
            windows_reset: cleared,
        }),
        Some("ineligible") | Some("unavailable") => Ok(RedeemOutcome {
            outcome: "no_credit",
            message: "No Claude reset available".into(),
            windows_reset: cleared,
        }),
        Some("cooldown") => Err("Claude resets are cooling down — try later".into()),
        other => Err(format!(
            "unexpected reset response (HTTP {status}, result {})",
            other.unwrap_or("<none>")
        )),
    }
}

/// Spends one banked Claude limit reset — irreversible; the UI confirms
/// first, and the POST is sent exactly once (never retried here).
pub async fn redeem_credit(
    provider_id: &str,
    credit_id: &str,
    redeem_request_id: Option<&str>,
) -> Result<RedeemOutcome, String> {
    let Some(grant_id) = parse_reset_credit_id(credit_id) else {
        return Err("unknown Claude reset".into());
    };
    // Route the redeem to the account whose card offered the reset — an
    // extra account's Use button must spend ITS grant, same routing the
    // usage fetch uses.
    let dir = if provider_id == ID {
        default_dir()
    } else {
        discover_extra_accounts()
            .into_iter()
            .find(|a| a.id == provider_id)
            .map(|a| a.dir)
            .ok_or_else(|| format!("unknown Claude account: {provider_id}"))?
    };
    let (access, _) = oauth_access(&dir).await?;
    let request_id = match redeem_request_id {
        Some(r) if valid_request_id(r) => r.to_string(),
        Some(_) => return Err("invalid redeem request id".into()),
        None => format!("pane-{}-{}", Utc::now().timestamp_millis(), std::process::id()),
    };
    // The reset endpoint is org-scoped; the profile is where the org
    // uuid lives.
    let resp = http()
        .get("https://api.anthropic.com/api/oauth/profile")
        .bearer_auth(&access)
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", CLAUDE_CLI_UA)
        .send()
        .await
        .map_err(|e| format!("profile request: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("profile request: HTTP {}", resp.status()));
    }
    let profile: Value = resp.json().await.map_err(|e| format!("profile parse: {e}"))?;
    let org = profile
        .pointer("/organization/uuid")
        .and_then(Value::as_str)
        .filter(|u| {
            !u.is_empty()
                && u.len() <= 128
                && u.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
        .ok_or("profile has no organization uuid")?;
    let resp = http()
        .post(format!(
            "https://api.anthropic.com/api/organizations/{org}/reset_rate_limits"
        ))
        .bearer_auth(&access)
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", CLAUDE_CLI_UA)
        .json(&json!({
            "program": "cedar_ember",
            "grant_id": grant_id,
            "request_id": request_id,
        }))
        .send()
        .await
        .map_err(|e| format!("reset request: {e}"))?;
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or_else(|_| json!({}));
    claude_redeem_outcome(status, &body)
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
    use super::{banked_resets, identity_from, parse_reset_credit_id, scoped_id_charset};
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

    /// The exact body the usage endpoint returns for Claude Code OAuth
    /// tokens today.
    #[test]
    fn banked_resets_ineligible_block_yields_no_row() {
        let usage = json!({"cedar_ember": {
            "eligible": false,
            "ineligible_reason": "surface",
            "at_limit": false,
            "exhausted": [],
            "grants": [],
            "next_grant_id": null,
            "weekly_resets_at": null,
            "cooldown_until": null,
            "event_props": null
        }});
        assert!(banked_resets(&usage).is_none());
    }

    #[test]
    fn banked_resets_absent_or_null_yields_no_row() {
        assert!(banked_resets(&json!({"five_hour": {}})).is_none());
        assert!(banked_resets(&json!({"cedar_ember": null})).is_none());
    }

    /// The live body jazii's account answers with the CLI User-Agent.
    #[test]
    fn banked_resets_live_body_offers_the_grant() {
        let usage = json!({"cedar_ember": {
            "eligible": true,
            "ineligible_reason": null,
            "at_limit": false,
            "exhausted": [],
            "grants": [{
                "id": "opus55-launch-promax-20260921",
                "label": "Claude Opus 5.5 launch: one usage-limit reset for Pro and Max",
                "resets_total": 1,
                "resets_left": 1,
                "starts_at": "2026-09-22T16:00:00+00:00",
                "ends_at": "2026-10-22T16:00:00+00:00",
                "clears": ["five_hour", "seven_day", "seven_day_overage_included"],
                "paused": false,
                "usable_now": true,
                "use_requires_limit": false,
                "percent_used": {"five_hour": 1, "seven_day": 23},
                "blocking": [],
                "arm": null
            }],
            "next_grant_id": "opus55-launch-promax-20260921",
            "weekly_resets_at": "2026-09-23T21:00:00+00:00",
            "cooldown_until": null,
            "event_props": {}
        }});
        let m = banked_resets(&usage).expect("eligible grant should yield a row");
        assert_eq!(m.kind, "resets");
        assert_eq!(m.value.as_deref(), Some("1"));
        let ends_ms = chrono::DateTime::parse_from_rfc3339("2026-10-22T16:00:00+00:00")
            .unwrap()
            .timestamp_millis();
        assert_eq!(m.resets_at, Some(ends_ms));
        // The server-selected grant is claimable — its id is what the
        // redeem POST needs.
        let credits = serde_json::from_str::<serde_json::Value>(&m.detail.unwrap()).unwrap();
        let list = credits.as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["id"], "opus55-launch-promax-20260921~1");
        assert_eq!(list[0]["expires_at"], ends_ms);
    }

    /// Only the grant the server would spend is claimable — no
    /// next_grant_id, paused, or an active cooldown leaves every credit
    /// display-only (no id → no Use button).
    #[test]
    fn banked_resets_only_the_spendable_grant_gets_an_id() {
        let grant = json!({
            "id": "opus55-launch-promax-20260921",
            "resets_total": 1, "resets_left": 1,
            "ends_at": "2026-10-22T16:00:00+00:00",
            "paused": false, "usable_now": true
        });
        let credit_id = |ember: serde_json::Value| {
            let m = banked_resets(&json!({"cedar_ember": ember})).unwrap();
            serde_json::from_str::<serde_json::Value>(&m.detail.unwrap()).unwrap()[0]
                .get("id")
                .cloned()
        };
        // next_grant_id null → display-only.
        let mut ember = json!({"eligible": true, "next_grant_id": null, "grants": [grant.clone()]});
        assert!(credit_id(ember.clone()).is_none());
        // paused → display-only.
        let mut paused = grant.clone();
        paused["paused"] = json!(true);
        ember = json!({"eligible": true, "next_grant_id": "opus55-launch-promax-20260921",
            "grants": [paused]});
        assert!(credit_id(ember.clone()).is_none());
        // cooldown_until in the future → display-only.
        ember = json!({"eligible": true, "next_grant_id": "opus55-launch-promax-20260921",
            "cooldown_until": "2999-01-01T00:00:00Z", "grants": [grant.clone()]});
        assert!(credit_id(ember.clone()).is_none());
        // A past cooldown doesn't bench anything.
        ember = json!({"eligible": true, "next_grant_id": "opus55-launch-promax-20260921",
            "cooldown_until": "2020-01-01T00:00:00Z", "grants": [grant]});
        assert_eq!(credit_id(ember), Some(json!("opus55-launch-promax-20260921~1")));
    }

    /// resets_left 2 banks two credits, but each claim spends one reset
    /// from the server-selected grant — only the first copy is claimable.
    #[test]
    fn banked_resets_one_claimable_credit_per_grant() {
        let usage = json!({"cedar_ember": {"eligible": true,
            "next_grant_id": "g-1",
            "grants": [{"id": "g-1", "resets_total": 2, "resets_left": 2,
                "ends_at": "2026-10-22T16:00:00+00:00",
                "paused": false, "usable_now": true}]}});
        let m = banked_resets(&usage).unwrap();
        assert_eq!(m.value.as_deref(), Some("2"));
        let credits = serde_json::from_str::<serde_json::Value>(&m.detail.unwrap()).unwrap();
        let list = credits.as_array().unwrap();
        assert_eq!(list.len(), 2);
        let with_id: Vec<_> = list.iter().filter(|c| c.get("id").is_some()).collect();
        assert_eq!(with_id.len(), 1);
        // resets_left 2 → the offered id is g-1~2; after one claim the
        // refreshed g-1~1 is a different id, so the popover shows it
        // as still available with a fresh idempotency key.
        assert_eq!(with_id[0]["id"], "g-1~2");
    }

    /// The claimable id is `{grant}~{resets_left}`; the redeem call
    /// recovers the bare grant by splitting on the LAST '~'.
    #[test]
    fn parse_reset_credit_id_requires_the_left_suffix() {
        assert_eq!(
            parse_reset_credit_id("opus55-launch-promax-20260921~1"),
            Some("opus55-launch-promax-20260921")
        );
        assert_eq!(parse_reset_credit_id("g-1~42"), Some("g-1"));
        // No suffix, a zero or non-numeric suffix, or a grant part that
        // fails the charset/length rules all reject the click.
        assert_eq!(parse_reset_credit_id("opus55-launch-promax-20260921"), None);
        assert_eq!(parse_reset_credit_id("g-1~0"), None);
        assert_eq!(parse_reset_credit_id("g-1~abc"), None);
        // No cap on how many resets a grant can bank.
        assert_eq!(parse_reset_credit_id("g-1~1001"), Some("g-1"));
        assert_eq!(parse_reset_credit_id("G_1~2"), None);
        assert_eq!(parse_reset_credit_id("~2"), None);
        // Multiple '~': the last wins, so the grant part keeps a '~'
        // and fails validation.
        assert_eq!(parse_reset_credit_id("g~1~2"), None);
    }

    #[test]
    fn banked_resets_drops_unusable_grants_and_expands_left() {
        let a = "2026-10-22T07:00:00Z";
        let b = "2026-11-01T00:00:00Z";
        let usage = json!({"cedar_ember": {"eligible": true, "grants": [
            {"id": "g-a", "resets_total": 2, "resets_left": 2, "ends_at": a},
            {"id": "g-b", "resets_total": 1, "resets_left": 0, "ends_at": b},
            {"id": "g-c", "resets_total": 1, "resets_left": 1, "ends_at": "not-a-date"},
            {"id": "",    "resets_total": 1, "resets_left": 1, "ends_at": b}
        ]}});
        let m = banked_resets(&usage).unwrap();
        assert_eq!(m.value.as_deref(), Some("2"));
        let a_ms = chrono::DateTime::parse_from_rfc3339(a).unwrap().timestamp_millis();
        assert_eq!(m.resets_at, Some(a_ms));
        let credits = serde_json::from_str::<serde_json::Value>(&m.detail.unwrap()).unwrap();
        assert!(credits
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["expires_at"] == a_ms));
        // No next_grant_id → nothing claimable.
        assert!(credits
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c.get("id").is_none()));
    }

    #[test]
    fn claude_redeem_outcome_maps_every_result() {
        use super::claude_redeem_outcome;
        let ok = |result: &str| claude_redeem_outcome(200, &json!({"result": result, "cleared": ["five_hour", "seven_day"]}));
        let o = ok("reset").unwrap();
        assert_eq!((o.outcome, o.windows_reset), ("success", 2));
        assert_eq!(o.message, "Claude limits reset");
        let o = ok("already_used").unwrap();
        assert_eq!(o.outcome, "success");
        let o = ok("not_limited").unwrap();
        assert_eq!(o.outcome, "nothing_to_reset");
        for r in ["ineligible", "unavailable"] {
            assert_eq!(ok(r).unwrap().outcome, "no_credit", "{r}");
        }
        assert!(ok("cooldown").is_err());
        for r in ["stamp_indeterminate", "reset_unconfirmed", "whatever"] {
            assert!(ok(r).is_err(), "{r}");
        }
        // Rate limit and non-2xx are errors; malformed bodies too.
        assert!(claude_redeem_outcome(429, &json!({})).is_err());
        assert!(claude_redeem_outcome(500, &json!({"result": "reset"})).is_err());
        assert!(claude_redeem_outcome(200, &json!({})).is_err());
    }

    #[test]
    fn banked_resets_eligible_with_zero_grants_shows_zero() {
        let usage = json!({"cedar_ember": {"eligible": true, "grants": []}});
        let m = banked_resets(&usage).expect("eligible with no grants still rows");
        assert_eq!(m.value.as_deref(), Some("0"));
        assert_eq!(m.resets_at, None);
    }

    #[test]
    fn banked_resets_left_is_clamped_to_total() {
        let usage = json!({"cedar_ember": {"eligible": true, "grants": [
            {"id": "g-1", "resets_total": 1, "resets_left": 5,
             "ends_at": "2026-10-22T07:00:00Z"}
        ]}});
        let m = banked_resets(&usage).unwrap();
        assert_eq!(m.value.as_deref(), Some("1"));
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

    #[test]
    fn keyless_dir_requires_the_full_shape_without_oauth() {
        use super::is_keyless_claude_dir;
        let dir = std::env::temp_dir().join(format!("pane-keyless-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("projects")).unwrap();
        std::fs::create_dir_all(dir.join("shell-snapshots")).unwrap();
        std::fs::write(dir.join("history.jsonl"), "").unwrap();

        // Full shape, no credentials file → keyless.
        assert!(is_keyless_claude_dir(&dir));

        // An OAuth login present → discover_extra_accounts' business.
        std::fs::write(dir.join(".credentials.json"), "{}").unwrap();
        assert!(!is_keyless_claude_dir(&dir));
        let _ = std::fs::remove_file(dir.join(".credentials.json"));

        // Missing history → not a Claude Code config dir.
        let _ = std::fs::remove_file(dir.join("history.jsonl"));
        assert!(!is_keyless_claude_dir(&dir));
        std::fs::write(dir.join("history.jsonl"), "").unwrap();

        // Missing shell-snapshots → some other tool's projects/ tree.
        let _ = std::fs::remove_dir_all(dir.join("shell-snapshots"));
        assert!(!is_keyless_claude_dir(&dir));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
