//! Droid (Factory.ai) — quota rows from the Droid CLI's own sign-in.
//!
//! The CLI keeps its login at `~/.factory/auth.v2.keyring`: one line of
//! `base64(iv):base64(tag):base64(ciphertext)` AES-256-GCM, the 32-byte
//! key base64'd inside a Windows generic credential
//! (`Factory CLI/auth-encryption-key`). When the keyring is off it falls
//! back to `auth.v2.file` + a plain `auth.v2.key` holding the same
//! base64 key. Everything is read-only — Pane never writes the files,
//! never calls any refresh endpoint, and never uses the refresh token.
//! The access token expires ~24 h after Droid last refreshed it; then
//! the card goes stale until the user runs `droid` again.

use super::{http_no_redirect, Metric, Snapshot};
use base64::Engine;
use serde_json::Value;
use std::path::PathBuf;

const ID: &str = "droid";
const NAME: &str = "Droid";

const CRED_TARGET: &str = "Factory CLI/auth-encryption-key";
const LIMITS_URL: &str = "https://api.factory.ai/api/billing/limits";
const ME_URL: &str = "https://api.factory.ai/api/app/auth/me";
const LEGACY_USAGE_URL: &str =
    "https://api.factory.ai/api/organization/subscription/usage?useCache=true";
const MAX_BODY: usize = 1 << 20; // 1 MiB — far above any quota response
const AUTH_FILE_CAP: u64 = 64 * 1024;

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;
const SESSION_MS: i64 = 5 * HOUR_MS;
const WEEK_MS: i64 = 7 * DAY_MS;
const MONTH_MS: i64 = 30 * DAY_MS;

const EXPIRED_MSG: &str = "Droid login expired. Open Droid to refresh it.";
const UNREADABLE_MSG: &str = "Couldn't read the Droid login. Run droid once and try again.";

pub async fn snapshot() -> Snapshot {
    match fetch().await {
        Ok(s) => s,
        Err(e) => Snapshot::error(ID, NAME, e),
    }
}

/// Fingerprint of the Droid account id so a CLI sign-in swap drops the
/// previous account's cached last-good card — same mechanism as
/// claude/codex/opencode/stepfun. Never the raw id or token.
pub fn default_identity() -> Option<String> {
    let login = read_login().ok().flatten()?;
    login.account_id.map(|id| super::key_fingerprint(&id))
}

/// `FACTORY_HOME_OVERRIDE`, when set, IS the `.factory` directory.
fn factory_home() -> Option<PathBuf> {
    if let Ok(over) = std::env::var("FACTORY_HOME_OVERRIDE") {
        let over = over.trim();
        if !over.is_empty() {
            return Some(PathBuf::from(over));
        }
    }
    dirs::home_dir().map(|h| h.join(".factory"))
}

// ---------------------------------------------------------------------------
// Local credential — decrypt in memory only, never stored or logged
// ---------------------------------------------------------------------------

/// The credential blob is the 32-byte key encoded as base64 (current
/// Droid) or hex text, in UTF-8 or UTF-16LE (keytar variants); a raw
/// 32-byte blob is also accepted. First decode yielding 32 bytes wins.
fn key_from_credential_blob(blob: &[u8]) -> Option<Vec<u8>> {
    if blob.len() == 32 {
        return Some(blob.to_vec());
    }
    let utf8 = String::from_utf8(blob.to_vec()).ok();
    let utf16 = if blob.len().is_multiple_of(2) {
        let units: Vec<u16> = blob
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16(&units).ok()
    } else {
        None
    };
    for text in [utf8, utf16].into_iter().flatten() {
        let text = text.trim_matches('\0').trim();
        for decoded in [
            base64::engine::general_purpose::STANDARD.decode(text).ok(),
            hex_decode(text),
        ]
        .into_iter()
        .flatten()
        {
            if decoded.len() == 32 {
                return Some(decoded);
            }
        }
    }
    None
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// AES-256-GCM open through Windows CNG. Droid writes a 16-byte IV,
/// which ring's fixed 12-byte-nonce API cannot take — CNG's
/// BCRYPT_CHAIN_MODE_GCM accepts any nonce length.
fn aes_gcm_decrypt(
    key: &[u8],
    iv: &[u8],
    tag: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, String> {
    use windows::Win32::Security::Cryptography::*;
    unsafe {
        let mut alg = BCRYPT_ALG_HANDLE::default();
        if BCryptOpenAlgorithmProvider(
            &mut alg,
            BCRYPT_AES_ALGORITHM,
            None,
            BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0),
        )
        .0 != 0
        {
            return Err("open AES provider failed".into());
        }
        let mut kh = BCRYPT_KEY_HANDLE::default();
        let ok = set_gcm_mode(alg) && BCryptGenerateSymmetricKey(alg, &mut kh, None, key, 0).0 == 0;
        if !ok {
            let _ = BCryptCloseAlgorithmProvider(alg, 0);
            return Err("prepare AES-GCM key failed".into());
        }
        let mut info = BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO {
            cbSize: std::mem::size_of::<BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO>() as u32,
            dwInfoVersion: BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO_VERSION,
            pbNonce: iv.as_ptr() as *mut u8,
            cbNonce: iv.len() as u32,
            pbTag: tag.as_ptr() as *mut u8,
            cbTag: tag.len() as u32,
            ..Default::default()
        };
        let mut out = vec![0u8; ciphertext.len()];
        let mut written = 0u32;
        let status = BCryptDecrypt(
            kh,
            Some(ciphertext),
            Some(&mut info as *mut _ as *const core::ffi::c_void),
            None,
            Some(&mut out),
            &mut written,
            BCRYPT_FLAGS(0),
        );
        let _ = BCryptDestroyKey(kh);
        let _ = BCryptCloseAlgorithmProvider(alg, 0);
        if status.0 != 0 {
            return Err("decrypt failed".into());
        }
        out.truncate(written as usize);
        Ok(out)
    }
}

/// Same one-shot GCM plumbing for the test-side encryptor.
#[cfg(test)]
fn aes_gcm_encrypt(key: &[u8], iv: &[u8], plaintext: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
    use windows::Win32::Security::Cryptography::*;
    unsafe {
        let mut alg = BCRYPT_ALG_HANDLE::default();
        if BCryptOpenAlgorithmProvider(
            &mut alg,
            BCRYPT_AES_ALGORITHM,
            None,
            BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0),
        )
        .0 != 0
        {
            return Err("open AES provider failed".into());
        }
        let mut kh = BCRYPT_KEY_HANDLE::default();
        let ok = set_gcm_mode(alg) && BCryptGenerateSymmetricKey(alg, &mut kh, None, key, 0).0 == 0;
        if !ok {
            let _ = BCryptCloseAlgorithmProvider(alg, 0);
            return Err("prepare AES-GCM key failed".into());
        }
        let mut tag = vec![0u8; 16];
        let mut info = BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO {
            cbSize: std::mem::size_of::<BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO>() as u32,
            dwInfoVersion: BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO_VERSION,
            pbNonce: iv.as_ptr() as *mut u8,
            cbNonce: iv.len() as u32,
            pbTag: tag.as_mut_ptr(),
            cbTag: tag.len() as u32,
            ..Default::default()
        };
        let mut out = vec![0u8; plaintext.len()];
        let mut written = 0u32;
        let status = BCryptEncrypt(
            kh,
            Some(plaintext),
            Some(&mut info as *mut _ as *const core::ffi::c_void),
            None,
            Some(&mut out),
            &mut written,
            BCRYPT_FLAGS(0),
        );
        let _ = BCryptDestroyKey(kh);
        let _ = BCryptCloseAlgorithmProvider(alg, 0);
        if status.0 != 0 {
            return Err("encrypt failed".into());
        }
        out.truncate(written as usize);
        Ok((out, tag))
    }
}

/// Point a fresh AES provider handle at GCM chaining — required before
/// BCryptGenerateSymmetricKey, else the auth-mode info is ignored.
unsafe fn set_gcm_mode(alg: windows::Win32::Security::Cryptography::BCRYPT_ALG_HANDLE) -> bool {
    use windows::Win32::Security::Cryptography::{BCryptSetProperty, BCRYPT_CHAINING_MODE};
    let gcm_wide: Vec<u16> = "ChainingModeGCM"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let gcm_bytes =
        unsafe { std::slice::from_raw_parts(gcm_wide.as_ptr() as *const u8, gcm_wide.len() * 2) };
    unsafe { BCryptSetProperty(alg.into(), BCRYPT_CHAINING_MODE, gcm_bytes, 0) }.0 == 0
}

/// One `iv:tag:ciphertext` line (each part base64) → decrypted bytes.
fn decrypt_auth_file(path: &std::path::Path, key: &[u8]) -> Result<Vec<u8>, String> {
    let line = super::read_small_text(path, AUTH_FILE_CAP, "Droid login")?;
    let parts: Vec<&str> = line.trim().split(':').collect();
    if parts.len() != 3 {
        return Err("malformed".into());
    }
    let decode = |s: &str| base64::engine::general_purpose::STANDARD.decode(s.trim());
    let (Ok(iv), Ok(tag), Ok(ct)) = (decode(parts[0]), decode(parts[1]), decode(parts[2])) else {
        return Err("malformed".into());
    };
    aes_gcm_decrypt(key, &iv, &tag, &ct)
}

/// Depth-first search for `access_token`/`accessToken`, like the Droid
/// CLI's own tolerant JSON walk — the field can sit inside `whoami`.
fn find_token(v: &Value) -> Option<String> {
    let obj = v.as_object()?;
    for (k, child) in obj {
        if (k == "access_token" || k == "accessToken") && child.is_string() {
            return child.as_str().map(str::to_string);
        }
        if let Some(found) = find_token(child) {
            return Some(found);
        }
    }
    None
}

/// JWT claims without a signature check — it is the user's own local
/// token; the API itself verifies on each call.
fn jwt_payload(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&raw).ok()
}

fn jwt_account_id(token: &str) -> Option<String> {
    let claims = jwt_payload(token)?;
    claims
        .get("id")
        .or_else(|| claims.get("sub"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Expiry from the token's own `exp`. A missing claim can't be proven
/// expired, so it is allowed through — the API still answers 401.
fn token_expired(token: &str, now_s: i64) -> bool {
    jwt_payload(token)
        .and_then(|c| c.get("exp").and_then(Value::as_i64))
        .is_some_and(|exp| exp <= now_s)
}

struct DroidLogin {
    access_token: String,
    account_id: Option<String>,
}

/// Decrypt the CLI sign-in. `Ok(None)` = nothing to read (not signed
/// in); `Err` = something exists but could not be opened.
fn read_login() -> Result<Option<DroidLogin>, String> {
    let Some(home) = factory_home() else {
        return Ok(None);
    };
    let keyring = home.join("auth.v2.keyring");
    let file = home.join("auth.v2.file");
    let plain = if keyring.is_file() {
        let key = super::read_windows_credential(CRED_TARGET)
            .and_then(|blob| key_from_credential_blob(&blob))
            .ok_or_else(|| UNREADABLE_MSG.to_string())?;
        Some(decrypt_auth_file(&keyring, &key)?)
    } else if file.is_file() {
        let raw = super::read_small_text(&home.join("auth.v2.key"), 4096, "Droid login key")?;
        let key = base64::engine::general_purpose::STANDARD
            .decode(raw.trim())
            .ok()
            .filter(|k| k.len() == 32)
            .ok_or_else(|| UNREADABLE_MSG.to_string())?;
        Some(decrypt_auth_file(&file, &key)?)
    } else {
        None
    };
    let Some(plain) = plain else { return Ok(None) };
    let json: Value = serde_json::from_slice(&plain).map_err(|_| UNREADABLE_MSG.to_string())?;
    let token = find_token(&json)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| UNREADABLE_MSG.to_string())?;
    let account_id = jwt_account_id(&token);
    Ok(Some(DroidLogin {
        access_token: token,
        account_id,
    }))
}

// ---------------------------------------------------------------------------
// API
// ---------------------------------------------------------------------------

async fn get_json(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    what: &str,
) -> Result<Value, String> {
    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        .header("x-factory-client", "web-app")
        .header("Origin", "https://app.factory.ai")
        .header("Referer", "https://app.factory.ai/")
        .send()
        .await
        .map_err(|e| format!("{what}: {}", e.without_url()))?;
    if resp.status().as_u16() == 401 || resp.status().as_u16() == 403 {
        return Err(EXPIRED_MSG.into());
    }
    if !resp.status().is_success() {
        return Err(format!("{what}: HTTP {}", resp.status()));
    }
    super::json_body(resp, MAX_BODY, what).await
}

/// The orb subscription's plan (string or `{name}`) with the
/// "Factory … Plan" wrapper stripped — "Factory Max Plan" → "Max" —
/// else the `factoryTier` capitalised ("max" → "Max").
fn plan_label(me: &Value) -> Option<String> {
    let sub = me.pointer("/organization/subscription")?;
    let plan = sub.pointer("/orbSubscription/plan");
    let name = plan.and_then(|p| {
        p.as_str()
            .or_else(|| p.get("name").and_then(Value::as_str))
            .map(str::to_string)
    });
    if let Some(name) = name {
        let name = name.trim();
        let name = name.strip_prefix("Factory ").unwrap_or(name);
        let name = name.strip_suffix(" Plan").unwrap_or(name);
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    let tier = sub.get("factoryTier").and_then(Value::as_str)?.trim();
    let mut chars = tier.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
}

/// windowEnd wins when it still lies ahead; secondsRemaining is the
/// window's own countdown and converts to a wall-clock reset.
fn window_reset(node: &Value, now_ms: i64) -> Option<i64> {
    if let Some(end) = node
        .get("windowEnd")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis())
    {
        if end > now_ms {
            return Some(end);
        }
    }
    node.get("secondsRemaining")
        .and_then(Value::as_f64)
        .filter(|s| *s > 0.0)
        .map(|s| now_ms + (s * 1000.0) as i64)
}

/// A quota window → a progress row. `used_percent` absent (and no reset
/// to prove the window exists) means the field genuinely isn't there.
fn window_metric(label: &str, node: &Value, period_ms: i64, now_ms: i64) -> Option<Metric> {
    let used = node.get("usedPercent").and_then(Value::as_f64)?;
    Some(
        Metric::progress(label, used.clamp(0.0, 100.0), None)
            .with_reset(window_reset(node, now_ms), Some(period_ms)),
    )
}

/// The token-rate-limits billing shape: Session/Weekly/Monthly standard
/// rows, then any core windows that actually carry usage, then the
/// extra-usage balance. None when the flag is off or `limits.standard`
/// is absent.
fn billing_metrics(doc: &Value, now_ms: i64) -> Option<Vec<Metric>> {
    if doc
        .get("usesTokenRateLimitsBilling")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return None;
    }
    let standard = doc.pointer("/limits/standard")?;
    let mut metrics = Vec::new();
    let windows = [
        ("Session", "fiveHour", SESSION_MS),
        ("Weekly", "weekly", WEEK_MS),
        ("Monthly", "monthly", MONTH_MS),
    ];
    for (label, key, period) in windows {
        if let Some(m) = standard
            .get(key)
            .and_then(|n| window_metric(label, n, period, now_ms))
        {
            metrics.push(m);
        }
    }
    // The core pool only earns rows when a window reports usage.
    if let Some(core) = doc.pointer("/limits/core") {
        for (label, key, period) in [
            ("Core session", "fiveHour", SESSION_MS),
            ("Core weekly", "weekly", WEEK_MS),
            ("Core monthly", "monthly", MONTH_MS),
        ] {
            let used = core
                .get(key)
                .and_then(|n| n.get("usedPercent"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            if used > 0.0 {
                if let Some(m) = core
                    .get(key)
                    .and_then(|n| window_metric(label, n, period, now_ms))
                {
                    metrics.push(m);
                }
            }
        }
    }
    let cents = doc
        .get("extraUsageBalanceCents")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if cents > 0.0 {
        metrics.push(Metric::text(
            "Extra usage",
            format!("${:.2}", cents / 100.0),
        ));
    }
    Some(metrics)
}

/// "185K / 200M tokens" — compact magnitudes like capacity::fmt_tokens.
fn compact_tokens(v: f64) -> String {
    if v >= 1e9 {
        format!("{:.2}B", v / 1e9)
    } else if v >= 1e6 {
        format!("{:.0}M", v / 1e6)
    } else if v >= 1e3 {
        format!("{:.0}K", v / 1e3)
    } else {
        format!("{}", v.round() as i64)
    }
}

/// The pre-rate-limits shape: one monthly row from standard.usedRatio.
fn legacy_metrics(doc: &Value, now_ms: i64) -> Vec<Metric> {
    let Some(usage) = doc.pointer("/usage/standard") else {
        return Vec::new();
    };
    let Some(ratio) = usage.get("usedRatio").and_then(Value::as_f64) else {
        return Vec::new();
    };
    let detail = match (
        usage.get("userTokens").and_then(Value::as_f64),
        usage.get("totalAllowance").and_then(Value::as_f64),
    ) {
        (Some(used), Some(total)) => Some(format!(
            "{} / {} tokens",
            compact_tokens(used),
            compact_tokens(total)
        )),
        _ => None,
    };
    let reset = doc
        .pointer("/usage/endDate")
        .and_then(Value::as_f64)
        .map(|ms| ms as i64)
        .filter(|ms| *ms > now_ms);
    vec![
        Metric::progress("Monthly", (ratio * 100.0).clamp(0.0, 100.0), detail)
            .with_reset(reset, None),
    ]
}

async fn fetch() -> Result<Snapshot, String> {
    let Some(home) = factory_home() else {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Droid not found on this PC.",
        ));
    };
    if !home.is_dir() {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Droid not found on this PC.",
        ));
    }
    let Some(login) = read_login().map_err(|_| UNREADABLE_MSG.to_string())? else {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Sign in to Droid (run `droid`) and try again.",
        ));
    };
    if token_expired(&login.access_token, chrono::Utc::now().timestamp()) {
        return Err(EXPIRED_MSG.into());
    }

    let client = http_no_redirect();
    let (limits, me) = tokio::join!(
        get_json(&client, LIMITS_URL, &login.access_token, "limits"),
        get_json(&client, ME_URL, &login.access_token, "account"),
    );
    let limits = limits?;
    let plan = me.ok().as_ref().and_then(plan_label);
    let now_ms = chrono::Utc::now().timestamp_millis();

    let metrics = match billing_metrics(&limits, now_ms) {
        Some(m) if !m.is_empty() => m,
        _ => {
            let doc = get_json(&client, LEGACY_USAGE_URL, &login.access_token, "usage").await?;
            let metrics = legacy_metrics(&doc, now_ms);
            if metrics.is_empty() {
                return Err("Droid usage format not recognised".into());
            }
            metrics
        }
    };
    Ok(Snapshot::ok(ID, NAME, plan, metrics))
}

// ---------------------------------------------------------------------------
// Local spend events — ~/.factory/sessions/<cwd-slug>/<uuid>.{settings.json,jsonl}
// ---------------------------------------------------------------------------
//
// Each session's settings.json carries `tokenUsage` — cumulative counts
// for THIS session only (`inclusiveTokenUsage` also counts child
// sessions, which have their own files — using it would double count).
// The jsonl has no per-message tokens, so the session total is split
// evenly across its assistant messages, each share dated at that
// message's timestamp. All strictly read-only.

use super::minimax::{file_stamp, FileStamp};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::time::SystemTime;

/// One assistant turn's even share of its session's tokenUsage.
/// `output` already folds thinking tokens; `cache_write` is
/// cacheCreationTokens.
#[derive(Clone)]
pub struct DroidSessionEvent {
    pub ts_ms: i64,
    pub model: String,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// settings.json is a small session metadata file — bound the read.
const SESSION_SETTINGS_CAP: u64 = 256 * 1024;
/// A single jsonl line this long is a transcript dump, not a message
/// line — skip it rather than feed serde megabytes.
const SESSION_LINE_CAP: usize = 4 * 1024 * 1024;

fn session_jsonl(settings: &Path) -> PathBuf {
    let name = settings
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    settings.with_file_name(format!("{}.jsonl", name.trim_end_matches(".settings.json")))
}

/// (ts_ms, message modelId) pairs for assistant messages, in file order.
fn assistant_messages(jsonl: &Path) -> Vec<(i64, Option<String>)> {
    let Ok(file) = std::fs::File::open(jsonl) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut reader = std::io::BufReader::new(file);
    use std::io::BufRead;
    loop {
        buf.clear();
        let Ok(n) = reader.read_line(&mut buf) else {
            break;
        };
        if n == 0 {
            break;
        }
        if buf.len() > SESSION_LINE_CAP || !buf.contains("\"assistant\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&buf) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let msg = v.get("message");
        if msg.and_then(|m| m.get("role")).and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(ts) = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        else {
            continue;
        };
        let model = msg
            .and_then(|m| m.get("modelId"))
            .and_then(Value::as_str)
            .map(str::to_string);
        out.push((ts.timestamp_millis(), model));
    }
    out
}

/// Decode one session into dated per-message shares of its tokenUsage.
/// A token-carrying session with no assistant messages attributes
/// everything to the settings file's own mtime.
fn session_events(settings: &Path, settings_mtime: SystemTime) -> Vec<DroidSessionEvent> {
    let Ok(raw) = super::read_small_text(settings, SESSION_SETTINGS_CAP, "Droid session") else {
        return Vec::new();
    };
    let Ok(doc) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    let u = doc.get("tokenUsage");
    let num = |k: &str| u.and_then(|t| t.get(k)).and_then(Value::as_f64).unwrap_or(0.0);
    let (input, output, thinking, cache_write, cache_read) = (
        num("inputTokens"),
        num("outputTokens"),
        num("thinkingTokens"),
        num("cacheCreationTokens"),
        num("cacheReadTokens"),
    );
    let total = input + output + thinking + cache_write + cache_read;
    if total <= 0.0 {
        return Vec::new();
    }
    let settings_model = doc.get("model").and_then(Value::as_str).unwrap_or_default();
    let msgs = assistant_messages(&session_jsonl(settings));
    let event = |ts_ms: i64, model: Option<String>, n: f64| DroidSessionEvent {
        ts_ms,
        model: model.unwrap_or_else(|| settings_model.to_string()),
        input: input / n,
        output: (output + thinking) / n,
        cache_read: cache_read / n,
        cache_write: cache_write / n,
    };
    if msgs.is_empty() {
        let ts_ms = settings_mtime
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        return vec![event(ts_ms, None, 1.0)];
    }
    let n = msgs.len() as f64;
    msgs.into_iter().map(|(ts, m)| event(ts, m, n)).collect()
}

type SessionCache = HashMap<PathBuf, (FileStamp, FileStamp, Vec<DroidSessionEvent>)>;

/// Token events across every session under `home`/sessions. Each session
/// is cached on its (settings, jsonl) stamps; a session untouched since
/// `cutoff` is skipped without opening it.
fn collect_from(home: &Path, cutoff: SystemTime, cache: &mut SessionCache) -> Vec<DroidSessionEvent> {
    let root = home.join("sessions");
    let Ok(slugs) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut sessions: Vec<PathBuf> = Vec::new();
    for slug in slugs.flatten() {
        let Ok(files) = std::fs::read_dir(slug.path()) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            if p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".settings.json"))
            {
                sessions.push(p);
            }
        }
    }
    sessions.sort();
    let live: HashSet<&PathBuf> = sessions.iter().collect();
    cache.retain(|k, _| live.contains(k));
    let cutoff_ms = cutoff
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let mut out = Vec::new();
    for settings in sessions {
        let s = file_stamp(&settings);
        let j = file_stamp(&session_jsonl(&settings));
        if s.0.max(j.0) < cutoff {
            continue;
        }
        let entry = cache
            .entry(settings.clone())
            .or_insert_with(|| ((SystemTime::UNIX_EPOCH, 0), (SystemTime::UNIX_EPOCH, 0), Vec::new()));
        if entry.0 != s || entry.1 != j {
            entry.2 = session_events(&settings, s.0);
            entry.0 = s;
            entry.1 = j;
        }
        out.extend(entry.2.iter().filter(|e| e.ts_ms >= cutoff_ms).cloned());
    }
    out
}

/// Dated token events from the Droid CLI's local session files, newest
/// store state each call. See `collect_from`.
pub fn collect_usage_events(cutoff: SystemTime) -> Vec<DroidSessionEvent> {
    static CACHE: Mutex<Option<SessionCache>> = Mutex::new(None);
    let Some(home) = factory_home() else {
        return Vec::new();
    };
    match CACHE.lock() {
        Ok(mut guard) => collect_from(&home, cutoff, guard.get_or_insert_with(HashMap::new)),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TEST_KEY: &[u8; 32] = &[7u8; 32];
    const TEST_IV: &[u8; 16] = &[3u8; 16];
    /// A fixed instant inside the fixtures' billing month.
    const NOW_MS: i64 = 1_791_676_800_000; // 2026-10-14T00:00:00Z
    const B64: base64::engine::general_purpose::GeneralPurpose =
        base64::engine::general_purpose::STANDARD;

    fn auth_line(key: &[u8; 32], iv: &[u8; 16], plaintext: &str) -> String {
        let (ct, tag) = aes_gcm_encrypt(key, iv, plaintext.as_bytes()).unwrap();
        format!("{}:{}:{}", B64.encode(iv), B64.encode(tag), B64.encode(ct))
    }

    fn write_auth(dir: &std::path::Path, name: &str, line: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, line).unwrap();
        path
    }

    #[test]
    fn decrypt_round_trips_and_rejects_tampering() {
        let dir = std::env::temp_dir().join(format!("pane-droid-{}", std::process::id()));
        let payload = r#"{"whoami":{"access_token":"h.p.s","x":1},"refresh_token":"r"}"#;
        let line = auth_line(TEST_KEY, TEST_IV, payload);
        let path = write_auth(&dir, "auth.v2.file", &line);
        let plain = decrypt_auth_file(&path, TEST_KEY).unwrap();
        let doc: Value = serde_json::from_slice(&plain).unwrap();
        assert_eq!(find_token(&doc).as_deref(), Some("h.p.s"));

        // A tampered tag fails the GCM auth check; a wrong key too.
        let bad_tag = format!(
            "{}:{}:{}",
            B64.encode(TEST_IV),
            B64.encode([9u8; 16]),
            line.rsplit(':').next().unwrap()
        );
        let path = write_auth(&dir, "auth.v2.bad", &bad_tag);
        assert!(decrypt_auth_file(&path, TEST_KEY).is_err());
        let good = auth_line(&[8u8; 32], TEST_IV, payload);
        let path = write_auth(&dir, "auth.v2.file", &good);
        assert!(decrypt_auth_file(&path, TEST_KEY).is_err());

        // Malformed files: wrong part count and bad base64.
        let path = write_auth(&dir, "auth.v2.file", "a:b");
        assert!(decrypt_auth_file(&path, TEST_KEY).is_err());
        let path = write_auth(&dir, "auth.v2.file", "!!:AA:AA");
        assert!(decrypt_auth_file(&path, TEST_KEY).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn credential_blob_decodes_every_spelling() {
        let key = [5u8; 32];
        let b64_key = B64.encode(key);
        assert_eq!(key_from_credential_blob(b64_key.as_bytes()).unwrap(), key);
        // UTF-16LE text blob.
        let utf16: Vec<u8> = b64_key
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(key_from_credential_blob(&utf16).unwrap(), key);
        // Hex text.
        let hex_key: String = key.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(key_from_credential_blob(hex_key.as_bytes()).unwrap(), key);
        // Raw 32-byte blob.
        assert_eq!(key_from_credential_blob(&key).unwrap(), key);
        // Wrong decoded length is not a key.
        assert!(key_from_credential_blob(B64.encode([5u8; 16]).as_bytes()).is_none());
        assert!(key_from_credential_blob(b"not-a-key").is_none());
    }

    /// The exact limits response captured from a real Factory Max account.
    fn limits_doc() -> Value {
        json!({
            "usesTokenRateLimitsBilling": true,
            "limits": {
                "standard": {
                    "fiveHour": {"usedPercent": 0, "windowEnd": null, "secondsRemaining": null},
                    "weekly": {"usedPercent": 0, "windowEnd": null, "secondsRemaining": null},
                    "monthly": {"usedPercent": 1, "windowEnd": "2026-10-29T11:52:48.647Z", "secondsRemaining": 1821434}
                },
                "core": {
                    "fiveHour": {"usedPercent": 0, "windowEnd": null, "secondsRemaining": null},
                    "weekly": {"usedPercent": 0, "windowEnd": null, "secondsRemaining": null},
                    "monthly": {"usedPercent": 0, "windowEnd": null, "secondsRemaining": null}
                }
            },
            "overagePreference": "droidCore",
            "canManageOverage": true,
            "extraUsageBalanceCents": 0,
            "extraUsageAllowed": true
        })
    }

    fn labels(metrics: &[Metric]) -> Vec<&str> {
        metrics.iter().map(|m| m.label.as_str()).collect()
    }

    #[test]
    fn billing_limits_map_to_session_weekly_monthly() {
        let metrics = billing_metrics(&limits_doc(), NOW_MS).unwrap();
        assert_eq!(labels(&metrics), ["Session", "Weekly", "Monthly"]);
        let monthly = &metrics[2];
        assert_eq!(monthly.used_percent, Some(1.0));
        let expected_reset = chrono::DateTime::parse_from_rfc3339("2026-10-29T11:52:48.647Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(monthly.resets_at, Some(expected_reset));
        assert_eq!(monthly.period_ms, Some(MONTH_MS));
        // Inactive windows still show their 0% bar, without a reset.
        assert_eq!(metrics[0].resets_at, None);
        assert_eq!(metrics[0].period_ms, Some(SESSION_MS));
        assert_eq!(metrics[1].resets_at, None);
        assert_eq!(metrics[1].period_ms, Some(WEEK_MS));
    }

    #[test]
    fn core_windows_and_extra_usage_rows_appear_when_used() {
        let mut doc = limits_doc();
        doc["limits"]["core"]["weekly"]["usedPercent"] = json!(5);
        doc["extraUsageBalanceCents"] = json!(1234);
        let metrics = billing_metrics(&doc, NOW_MS).unwrap();
        assert_eq!(
            labels(&metrics),
            ["Session", "Weekly", "Monthly", "Core weekly", "Extra usage"]
        );
        assert_eq!(metrics[3].used_percent, Some(5.0));
        assert_eq!(metrics[4].value.as_deref(), Some("$12.34"));
    }

    #[test]
    fn seconds_remaining_fills_in_when_window_end_is_null() {
        let mut doc = limits_doc();
        doc["limits"]["standard"]["weekly"]["secondsRemaining"] = json!(3600);
        let metrics = billing_metrics(&doc, NOW_MS).unwrap();
        assert_eq!(metrics[1].resets_at, Some(NOW_MS + 3_600_000));
    }

    #[test]
    fn unrecognised_limits_shape_falls_through() {
        assert!(billing_metrics(&json!({"usesTokenRateLimitsBilling": false}), NOW_MS).is_none());
        assert!(billing_metrics(&json!({"usesTokenRateLimitsBilling": true}), NOW_MS).is_none());
    }

    #[test]
    fn legacy_shape_maps_one_monthly_row() {
        let doc = json!({
            "usage": {
                "startDate": 1790491732949i64,
                "endDate": null,
                "standard": {"userTokens": 184919, "totalAllowance": 200000000,
                             "usedRatio": 0.000924595},
                "premium": {}
            }
        });
        let metrics = legacy_metrics(&doc, NOW_MS);
        assert_eq!(labels(&metrics), ["Monthly"]);
        let m = &metrics[0];
        assert!((m.used_percent.unwrap() - 0.0924595).abs() < 1e-6);
        assert_eq!(m.detail.as_deref(), Some("185K / 200M tokens"));
        assert_eq!(m.resets_at, None);
    }

    #[test]
    fn plan_label_uses_plan_name_then_tier() {
        let me = json!({"organization": {"subscription": {
            "factoryTier": "max",
            "orbSubscription": {"plan": {"name": "Factory Max Plan"}}}}});
        assert_eq!(plan_label(&me).as_deref(), Some("Max"));
        // Plan may arrive as a bare string.
        let me = json!({"organization": {"subscription": {
            "orbSubscription": {"plan": "Factory Pro Plan"}}}});
        assert_eq!(plan_label(&me).as_deref(), Some("Pro"));
        // No plan object → capitalised tier.
        let me = json!({"organization": {"subscription": {"factoryTier": "max"}}});
        assert_eq!(plan_label(&me).as_deref(), Some("Max"));
    }

    fn unsigned_jwt(claims: Value) -> String {
        let seg = |v: &Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
        };
        format!("{}.{}.x", seg(&json!({"alg": "none"})), seg(&claims))
    }

    #[test]
    fn jwt_expiry_and_account_id_come_from_the_payload() {
        let future = unsigned_jwt(json!({"exp": 4_000_000_000i64, "id": "acct-1"}));
        assert!(!token_expired(&future, 1_800_000_000));
        assert_eq!(jwt_account_id(&future).as_deref(), Some("acct-1"));

        let past = unsigned_jwt(json!({"exp": 1_700_000_000i64}));
        assert!(token_expired(&past, 1_800_000_000));

        // No exp cannot be proven expired — the API's 401 decides.
        let no_exp = unsigned_jwt(json!({"sub": "acct-2"}));
        assert!(!token_expired(&no_exp, 4_000_000_000));
        assert_eq!(jwt_account_id(&no_exp).as_deref(), Some("acct-2"));
    }

    // --- session spend fixtures -------------------------------------------

    fn session_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pane-droid-spend-{tag}-{}", std::process::id()))
    }

    fn write_session(home: &Path, slug: &str, sid: &str, settings: &Value, jsonl: &[&str]) {
        let dir = home.join("sessions").join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{sid}.settings.json")),
            serde_json::to_string(settings).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join(format!("{sid}.jsonl")), jsonl.join("\n")).unwrap();
    }

    fn usage(i: f64, o: f64, t: f64, cw: f64, cr: f64) -> Value {
        json!({"inputTokens": i, "outputTokens": o, "thinkingTokens": t,
               "cacheCreationTokens": cw, "cacheReadTokens": cr})
    }

    #[test]
    fn session_tokens_split_evenly_across_dated_messages() {
        let home = session_dir("split");
        let _ = std::fs::remove_dir_all(&home);
        let settings = json!({
            "model": "claude-opus-5-5",
            "tokenUsage": usage(300.0, 60.0, 30.0, 90.0, 120.0),
            // Child sessions' own files carry their share — never count it twice.
            "inclusiveTokenUsage": usage(900_000.0, 900_000.0, 0.0, 0.0, 0.0),
        });
        write_session(
            &home,
            "proj",
            "s1",
            &settings,
            &[
                // Noise: a non-message line, a user turn, garbage, a missing ts.
                "{\"type\":\"other\",\"timestamp\":\"2026-10-01T09:00:00Z\"}",
                "{\"type\":\"message\",\"timestamp\":\"2026-10-01T09:30:00Z\",\"message\":{\"role\":\"user\",\"modelId\":\"x\"}}",
                "not json at all \"assistant\"",
                "{\"type\":\"message\",\"message\":{\"role\":\"assistant\"}}",
                "{\"type\":\"message\",\"timestamp\":\"2026-10-01T10:00:00Z\",\"message\":{\"role\":\"assistant\",\"modelId\":\"gpt-6-sol\"}}",
                "{\"type\":\"message\",\"timestamp\":\"2026-10-02T10:00:00Z\",\"message\":{\"role\":\"assistant\",\"modelId\":\"claude-opus-5-5\"}}",
                "{\"type\":\"message\",\"timestamp\":\"2026-10-02T12:00:00Z\",\"message\":{\"role\":\"assistant\"}}",
            ],
        );
        // Total = 300+60+30+90+120 = 600 → 200 per message.
        let mut cache = SessionCache::new();
        let events = collect_from(&home, SystemTime::UNIX_EPOCH, &mut cache);
        assert_eq!(events.len(), 3);
        let mut total = 0.0;
        for e in &events {
            assert_eq!(e.input, 100.0);
            assert_eq!(e.output, 30.0); // output + thinking
            assert_eq!(e.cache_write, 30.0);
            assert_eq!(e.cache_read, 40.0);
            total += e.input + e.output + e.cache_read + e.cache_write;
        }
        assert_eq!(total, 600.0);
        assert_eq!(events[0].model, "gpt-6-sol");
        assert_eq!(events[1].model, "claude-opus-5-5");
        assert_eq!(events[2].model, "claude-opus-5-5"); // no modelId → settings model
        let day = |ms: i64| {
            chrono::DateTime::from_timestamp_millis(ms)
                .unwrap()
                .date_naive()
        };
        assert_eq!(day(events[0].ts_ms).to_string(), "2026-10-01");
        assert_eq!(day(events[1].ts_ms).to_string(), "2026-10-02");
        assert_eq!(day(events[2].ts_ms).to_string(), "2026-10-02");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn session_without_assistant_messages_uses_settings_mtime() {
        let home = session_dir("nomsg");
        let _ = std::fs::remove_dir_all(&home);
        write_session(
            &home,
            "proj",
            "s2",
            &json!({"model": "gpt-6-astra", "tokenUsage": usage(50.0, 20.0, 0.0, 0.0, 0.0)}),
            &["{\"type\":\"message\",\"timestamp\":\"2026-10-01T10:00:00Z\",\"message\":{\"role\":\"user\"}}"],
        );
        let mut cache = SessionCache::new();
        let events = collect_from(&home, SystemTime::UNIX_EPOCH, &mut cache);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].model, "gpt-6-astra");
        assert_eq!(events[0].input, 50.0);
        // Timestamp is the settings file's mtime — always present.
        let st_mtime = file_stamp(&home.join("sessions/proj/s2.settings.json"))
            .0
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        assert_eq!(events[0].ts_ms, st_mtime);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn zero_token_and_stale_sessions_are_ignored() {
        let home = session_dir("empty");
        let _ = std::fs::remove_dir_all(&home);
        write_session(
            &home,
            "proj",
            "z1",
            &json!({"model": "auto", "tokenUsage": usage(0.0, 0.0, 0.0, 0.0, 0.0)}),
            &["{\"type\":\"message\",\"timestamp\":\"2026-10-01T10:00:00Z\",\"message\":{\"role\":\"assistant\"}}"],
        );
        let mut cache = SessionCache::new();
        let events = collect_from(&home, SystemTime::UNIX_EPOCH, &mut cache);
        assert!(events.is_empty());
        // A cutoff past every file's mtime reads nothing.
        let events = collect_from(&home, SystemTime::now(), &mut cache);
        assert!(events.is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Decode the machine's real `~/.factory/sessions` with no cutoff and
    /// bucket by UTC day — compare with `python %TEMP%\droid-spend-ref.py`.
    /// Prints counts and model names only, never session content.
    /// `cargo test --lib droid_real -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn droid_real_sessions_match_probe_totals() {
        let events = collect_usage_events(SystemTime::UNIX_EPOCH);
        let mut days: std::collections::BTreeMap<String, (f64, HashMap<String, f64>)> =
            std::collections::BTreeMap::new();
        for e in &events {
            let day = chrono::DateTime::from_timestamp_millis(e.ts_ms)
                .unwrap()
                .date_naive()
                .to_string();
            let tokens = e.input + e.output + e.cache_read + e.cache_write;
            let ent = days.entry(day).or_default();
            ent.0 += tokens;
            *ent.1.entry(e.model.clone()).or_default() += tokens;
        }
        eprintln!("events: {}", events.len());
        for (d, (t, models)) in &days {
            let mut ms: Vec<_> = models.iter().collect();
            ms.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap());
            eprintln!("  {d} {}", t.round() as i64);
            for (m, v) in ms {
                eprintln!("      {m} {}", v.round() as i64);
            }
        }
    }
}
