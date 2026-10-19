//! Droid (Factory.ai) — quota rows from the Droid CLI's own sign-in.
//!
//! The CLI keeps its login at `~/.factory/auth.v2.keyring`: one line of
//! `base64(iv):base64(tag):base64(ciphertext)` AES-256-GCM, the 32-byte
//! key base64'd inside a Windows generic credential
//! (`Factory CLI/auth-encryption-key`). When the keyring is off it falls
//! back to `auth.v2.file` + a plain `auth.v2.key` holding the same
//! base64 key. When the login is expired (or within five minutes of it)
//! Pane renews it itself through WorkOS — the same rotating refresh the
//! CLI does — and writes the new pair back into the same encrypted file,
//! keeping a `.pane-bak`. All of it gated on Droid not running: no
//! `droid.exe` process and no fresh `auth.v2.write.lock`, because a
//! refresh token rotates on use and a CLI holding the old one would be
//! signed out. When a refresh can't safely land the card goes stale
//! until the user runs `droid` again (~24 h access-token life).

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

/// Refresh lands the same rotating call the Droid CLI makes: WorkOS
/// `user_management/authenticate` with a form-encoded refresh grant.
/// The client id normally comes from the access token's own JWT claim;
/// this is the production fallback the CLI ships with.
const WORKOS_AUTHENTICATE_URL: &str = "https://api.workos.com/user_management/authenticate";
const WORKOS_CLIENT_ID: &str = "client_01HNM792M5G5G1A2THWPXKFMXB";
/// Refresh a token this close to expiry instead of letting it lapse.
const REFRESH_AHEAD_S: i64 = 300;
/// Any failed refresh pauses further attempts for this long.
const TRANSIENT_BACKOFF_MS: i64 = 30 * 60_000;
/// Droid holds `auth.v2.write.lock` while writing; a crashed droid can
/// leave it behind, so only a lock this fresh still counts as held.
const WRITE_LOCK_STALE_S: u64 = 120;

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;
const SESSION_MS: i64 = 5 * HOUR_MS;
const WEEK_MS: i64 = 7 * DAY_MS;
const MONTH_MS: i64 = 30 * DAY_MS;

pub(crate) const EXPIRED_MSG: &str = "Droid login expired. Open Droid to refresh it.";
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

/// Same one-shot GCM plumbing for writing the refreshed pair back —
/// Droid's file format, so the write needs the same seal as the read.
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
    decrypt_auth_line(&line, key)
}

/// The same decode without the file read — the refresh path holds the
/// raw line already for its pre-commit "unchanged" check.
fn decrypt_auth_line(line: &str, key: &[u8]) -> Result<Vec<u8>, String> {
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

/// The token's own `exp` claim in seconds — absent when the JWT has none.
fn token_exp_s(token: &str) -> Option<i64> {
    jwt_payload(token)?.get("exp")?.as_i64()
}

/// The access token still has more than REFRESH_AHEAD_S to live — or no
/// `exp` claim at all (can't be proven stale). Pure for tests.
fn still_fresh(access: &str, now_s: i64) -> bool {
    token_exp_s(access)
        .map(|exp| exp > now_s + REFRESH_AHEAD_S)
        .unwrap_or(true)
}

struct DroidLogin {
    access_token: String,
    account_id: Option<String>,
}

/// Everything the refresh path needs about the on-disk login: which file
/// was read (keyring vs keyfile variant), its key, the raw `iv:tag:ct`
/// line and (mtime, len) stamp taken at read time — the pre-commit
/// "unchanged" check compares against them — and the decrypted doc.
struct AuthFile {
    path: PathBuf,
    key: Vec<u8>,
    line: String,
    stamp: FileStamp,
    doc: Value,
}

/// Open whichever auth file exists and decrypt it. `Ok(None)` = nothing
/// to read (not signed in); `Err` = something exists but could not be
/// opened.
fn read_auth_file(home: &Path) -> Result<Option<AuthFile>, String> {
    let keyring = home.join("auth.v2.keyring");
    let file = home.join("auth.v2.file");
    let (path, key) = if keyring.is_file() {
        let key = super::read_windows_credential(CRED_TARGET)
            .and_then(|blob| key_from_credential_blob(&blob))
            .ok_or_else(|| UNREADABLE_MSG.to_string())?;
        (keyring, key)
    } else if file.is_file() {
        let raw = super::read_small_text(&home.join("auth.v2.key"), 4096, "Droid login key")?;
        let key = base64::engine::general_purpose::STANDARD
            .decode(raw.trim())
            .ok()
            .filter(|k| k.len() == 32)
            .ok_or_else(|| UNREADABLE_MSG.to_string())?;
        (file, key)
    } else {
        return Ok(None);
    };
    let stamp = file_stamp(&path);
    let line = super::read_small_text(&path, AUTH_FILE_CAP, "Droid login")?;
    let plain = decrypt_auth_line(&line, &key)?;
    let doc: Value = serde_json::from_slice(&plain).map_err(|_| UNREADABLE_MSG.to_string())?;
    Ok(Some(AuthFile {
        path,
        key,
        line,
        stamp,
        doc,
    }))
}

/// Decrypt the CLI sign-in. `Ok(None)` = nothing to read (not signed
/// in); `Err` = something exists but could not be opened.
fn read_login() -> Result<Option<DroidLogin>, String> {
    let Some(home) = factory_home() else {
        return Ok(None);
    };
    let Some(auth) = read_auth_file(&home)? else {
        return Ok(None);
    };
    let token = find_token(&auth.doc)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| UNREADABLE_MSG.to_string())?;
    let account_id = jwt_account_id(&token);
    Ok(Some(DroidLogin {
        access_token: token,
        account_id,
    }))
}

// ---------------------------------------------------------------------------
// Login refresh — the same rotating WorkOS grant the CLI runs. Every
// precondition is offline and checked twice (before the call and right
// before the write): a refresh token burns on use, so landing one while
// a `droid` process still caches the old pair would sign the user out.
// ---------------------------------------------------------------------------

/// Whether any enumerated image name is `droid.exe` (case-insensitive).
/// Pure so tests can feed name lists without a process table.
fn droid_running_in<'a>(mut names: impl Iterator<Item = &'a str>) -> bool {
    names.any(|n| n.eq_ignore_ascii_case("droid.exe"))
}

/// Names of every running process image via ToolHelp. `None` = the
/// enumeration failed — callers must fail closed (treat droid as
/// running) rather than risk rotating a token a live CLI still holds.
fn process_names() -> Option<Vec<String>> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut names = Vec::new();
        let mut entry = PROCESSENTRY32W::default();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                names.push(String::from_utf16_lossy(&entry.szExeFile[..end]));
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        Some(names)
    }
}

fn droid_running() -> bool {
    process_names()
        .map(|names| droid_running_in(names.iter().map(String::as_str)))
        .unwrap_or(true)
}

/// `auth.v2.write.lock` mtime → does it block a write? Absent = free; a
/// crashed droid's stale lock (older than WRITE_LOCK_STALE_S) = free; a
/// future-dated mtime counts as held.
fn write_lock_blocks(mtime: Option<SystemTime>, now: SystemTime) -> bool {
    match mtime {
        None => false,
        Some(t) => now
            .duration_since(t)
            .map(|d| d.as_secs() < WRITE_LOCK_STALE_S)
            .unwrap_or(true),
    }
}

/// Failed-refresh backoff, process-wide. A transient failure (network,
/// 5xx, a lost write) pauses all attempts for TRANSIENT_BACKOFF_MS; a
/// permanent rejection (WorkOS 400/401, or an `invalid_grant` body —
/// and a consumed token we couldn't land) bars only that refresh token's
/// fingerprint forever. The CLI does the same, and a re-login rotates
/// the token, so the new fingerprint clears it naturally.
struct RefreshBackoff {
    transient_until_ms: i64,
    rejected_fp: Option<String>,
}

static REFRESH_BACKOFF: Mutex<RefreshBackoff> = Mutex::new(RefreshBackoff {
    transient_until_ms: 0,
    rejected_fp: None,
});

/// One refresh at a time process-wide: overlapping callers would stage
/// the same `<live>.pane-tmp` (the second clobbering the first's) and
/// send the SAME refresh token to WorkOS — the loser eats invalid_grant
/// and the winner's commit can lose its tmp, burning the rotated pair.
static REFRESH_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn backoff_allows(b: &RefreshBackoff, fp: &str, now_ms: i64) -> bool {
    now_ms >= b.transient_until_ms && b.rejected_fp.as_deref() != Some(fp)
}

fn backoff_record(b: &mut RefreshBackoff, permanent_fp: Option<&str>, now_ms: i64) {
    match permanent_fp {
        Some(fp) => b.rejected_fp = Some(fp.to_string()),
        None => b.transient_until_ms = now_ms + TRANSIENT_BACKOFF_MS,
    }
}

fn backoff_note(permanent_fp: Option<&str>) {
    if let Ok(mut g) = REFRESH_BACKOFF.lock() {
        backoff_record(&mut g, permanent_fp, chrono::Utc::now().timestamp_millis());
    }
}

/// WorkOS audience for the refresh grant: the access token's `client_id`
/// claim when it looks like one, else the CLI's baked-in production id.
fn workos_client_id(access: &str) -> String {
    jwt_payload(access)
        .and_then(|c| c.get("client_id").and_then(Value::as_str).map(str::to_string))
        .filter(|id| id.starts_with("client_"))
        .unwrap_or_else(|| WORKOS_CLIENT_ID.to_string())
}

/// Clone of the decrypted doc with only the token pair swapped —
/// active_organization_id, whoami, and any unknown keys untouched. None
/// when the doc has no top-level refresh_token to replace.
fn refreshed_doc(old: &Value, access: &str, refresh: &str) -> Option<Value> {
    let mut doc = old.clone();
    let obj = doc.as_object_mut()?;
    if old.get("refresh_token").and_then(Value::as_str).is_none() {
        return None;
    }
    obj.insert("access_token".into(), Value::String(access.into()));
    obj.insert("refresh_token".into(), Value::String(refresh.into()));
    Some(doc)
}

/// The doc re-encrypted into Droid's one-line `iv:tag:ct` format with a
/// fresh 16-byte IV (no trailing newline, matching the file's shape).
fn encrypt_auth_line(key: &[u8], doc: &Value) -> Result<String, String> {
    let plaintext = serde_json::to_vec(doc).map_err(|_| "serialize".to_string())?;
    let mut iv = [0u8; 16];
    getrandom::getrandom(&mut iv).map_err(|_| "entropy".to_string())?;
    let (ct, tag) = aes_gcm_encrypt(key, &iv, &plaintext)?;
    let enc = base64::engine::general_purpose::STANDARD;
    Ok(format!("{}:{}:{}", enc.encode(iv), enc.encode(tag), enc.encode(ct)))
}

enum RefreshOutcome {
    /// WorkOS rotated the pair — both tokens are non-empty strings.
    Rotated { access: String, refresh: String },
    /// Network error, 5xx, unparseable — nothing was consumed.
    Transient,
    /// 400/401 or an `invalid_grant` body — this refresh token is dead.
    Rejected,
}

/// The refresh grant itself. Never logs token material.
async fn workos_refresh(refresh_token: &str, client_id: &str) -> RefreshOutcome {
    let resp = super::http()
        .post(WORKOS_AUTHENTICATE_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ])
        .send()
        .await;
    let Ok(resp) = resp else {
        return RefreshOutcome::Transient;
    };
    let success = resp.status().is_success();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    if !success {
        return if matches!(status, 400 | 401) || body.contains("invalid_grant") {
            RefreshOutcome::Rejected
        } else {
            RefreshOutcome::Transient
        };
    }
    let Ok(v) = serde_json::from_str::<Value>(&body) else {
        return RefreshOutcome::Transient;
    };
    let pick = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    match (pick("access_token"), pick("refresh_token")) {
        (Some(access), Some(refresh)) => RefreshOutcome::Rotated { access, refresh },
        _ => RefreshOutcome::Transient,
    }
}

fn is_regular_file(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path)
        .ok()
        .is_some_and(|m| m.is_file() && !m.file_type().is_symlink())
}

/// Proof the refreshed file can replace the live one BEFORE the rotating
/// call: any leftover or planted file at the predictable tmp path is
/// removed outright (a symlink or hardlink redirect is never written
/// through), a fresh tmp is created with create_new and owner-locked, and
/// it stays staged until commit so nothing can be planted in the gap.
/// Drop removes the tmp when the refresh never lands. Same staging model
/// as claude.rs's credential write-back.
struct StagedTmp(PathBuf);

impl StagedTmp {
    fn write(&self, contents: &str) -> std::io::Result<()> {
        std::fs::write(&self.0, contents)
    }
    fn commit(self, live: &Path) -> std::io::Result<()> {
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

fn stage_auth_tmp(live: &Path) -> Option<StagedTmp> {
    if !is_regular_file(live) {
        return None;
    }
    let mut tmp_os = live.as_os_str().to_os_string();
    tmp_os.push(".pane-tmp");
    let tmp = PathBuf::from(tmp_os);
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

/// Keep a copy of the CLI's own file before touching it, so a bad write
/// can never cost the user their login. A planted symlink at the backup
/// path is unlinked first — never write through it.
fn backup_auth_file(live: &Path) {
    let mut bak_os = live.as_os_str().to_os_string();
    bak_os.push(".pane-bak");
    let bak = PathBuf::from(bak_os);
    if std::fs::symlink_metadata(&bak)
        .ok()
        .is_some_and(|m| m.file_type().is_symlink())
    {
        let _ = std::fs::remove_file(&bak);
    }
    let _ = std::fs::copy(live, &bak);
}

/// Everything decided before the WorkOS call: droid not running, no fresh
/// write lock, a doc carrying both top-level tokens, backoff permitting,
/// and the staged tmp already created. `None` = do not even call out.
struct PreparedRefresh {
    auth: AuthFile,
    staged: StagedTmp,
    refresh_token: String,
    client_id: String,
    fp: String,
}

fn prepare_refresh(home: &Path, droid_is_running: bool, now_ms: i64) -> Option<PreparedRefresh> {
    if droid_is_running {
        return None;
    }
    let lock = home.join("auth.v2.write.lock");
    let lock_mtime = match std::fs::metadata(&lock) {
        Ok(m) => Some(m.modified().unwrap_or_else(|_| SystemTime::now())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(SystemTime::now()), // can't stat → treat as held
    };
    if write_lock_blocks(lock_mtime, SystemTime::now()) {
        return None;
    }
    let auth = read_auth_file(home).ok().flatten()?;
    let refresh = auth
        .doc
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?
        .to_string();
    let access = auth
        .doc
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?
        .to_string();
    let fp = super::key_fingerprint(&refresh);
    {
        let guard = REFRESH_BACKOFF.lock().ok()?;
        if !backoff_allows(&guard, &fp, now_ms) {
            return None;
        }
    }
    let client_id = workos_client_id(&access);
    let staged = stage_auth_tmp(&auth.path)?;
    Some(PreparedRefresh {
        auth,
        staged,
        refresh_token: refresh,
        client_id,
        fp,
    })
}

/// Land the rotated pair: replace only the two token fields, re-encrypt
/// into the staged tmp, verify the read-back round-trips to the exact
/// doc, then — only if droid still isn't running and the live file is
/// unchanged since the read (stamp AND raw line) — back it up and rename
/// over it. Any abort leaves the tmp to Drop and the live file alone.
fn commit_refresh(
    prep: PreparedRefresh,
    access: &str,
    refresh: &str,
    droid_is_running: bool,
) -> Option<String> {
    let new_doc = refreshed_doc(&prep.auth.doc, access, refresh)?;
    let line = encrypt_auth_line(&prep.auth.key, &new_doc).ok()?;
    prep.staged.write(&line).ok()?;
    let verified = decrypt_auth_file(&prep.staged.0, &prep.auth.key)
        .ok()
        .and_then(|p| serde_json::from_slice::<Value>(&p).ok())
        .is_some_and(|v| v == new_doc);
    if !verified {
        return None;
    }
    if droid_is_running {
        return None;
    }
    let unchanged = file_stamp(&prep.auth.path) == prep.auth.stamp
        && super::read_small_text(&prep.auth.path, AUTH_FILE_CAP, "Droid login")
            .ok()
            .as_deref()
            == Some(prep.auth.line.as_str());
    if !unchanged {
        return None;
    }
    backup_auth_file(&prep.auth.path);
    prep.staged.commit(&prep.auth.path).ok()?;
    Some(access.to_string())
}

/// The refresh itself. `Some(new access token)` only when the rotated
/// pair actually landed in the auth file; every failure path returns
/// None (and records the right backoff) without logging token material.
async fn try_refresh(home: &Path) -> Option<String> {
    let _gate = REFRESH_GATE.lock().await;
    // Another caller may have landed a refresh while we queued — and so
    // may the user running droid. Re-read: a no-longer-expiring token
    // means there's nothing to do, and reusing the file's refresh_token
    // would burn it.
    let current = read_auth_file(home)
        .ok()
        .flatten()?
        .doc
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?
        .to_string();
    if still_fresh(&current, chrono::Utc::now().timestamp()) {
        return Some(current);
    }
    let prep = prepare_refresh(home, droid_running(), chrono::Utc::now().timestamp_millis())?;
    match workos_refresh(&prep.refresh_token, &prep.client_id).await {
        RefreshOutcome::Rotated { access, refresh } => {
            let fp = prep.fp.clone();
            match commit_refresh(prep, &access, &refresh, droid_running()) {
                Some(t) => Some(t),
                // The call succeeded, so the old refresh token is already
                // burned server-side — bar its fingerprint so the next
                // scan doesn't hammer a dead grant.
                None => {
                    backoff_note(Some(fp.as_str()));
                    None
                }
            }
        }
        RefreshOutcome::Rejected => {
            backoff_note(Some(prep.fp.as_str()));
            None
        }
        RefreshOutcome::Transient => {
            backoff_note(None);
            None
        }
    }
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
    let now_s = chrono::Utc::now().timestamp();
    let expired = token_expired(&login.access_token, now_s);
    let mut access = login.access_token;
    if expired || token_exp_s(&access).is_some_and(|e| e <= now_s + REFRESH_AHEAD_S) {
        match try_refresh(&home).await {
            Some(t) => access = t,
            // Genuinely expired and unrenewable → the quiet-expired path
            // (#275); merely near expiry can keep using the old token.
            None if expired => return Err(EXPIRED_MSG.into()),
            None => {}
        }
    }

    let client = http_no_redirect();
    let (limits, me) = tokio::join!(
        get_json(&client, LIMITS_URL, &access, "limits"),
        get_json(&client, ME_URL, &access, "account"),
    );
    let limits = limits?;
    let plan = me.ok().as_ref().and_then(plan_label);
    let now_ms = chrono::Utc::now().timestamp_millis();

    let metrics = match billing_metrics(&limits, now_ms) {
        Some(m) if !m.is_empty() => m,
        _ => {
            let doc = get_json(&client, LEGACY_USAGE_URL, &access, "usage").await?;
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

/// Parse one jsonl line as an assistant message; anything else is ignored.
fn push_message_line(bytes: &[u8], out: &mut Vec<(i64, Option<String>)>) {
    let mut b = bytes;
    while matches!(b.last(), Some(b'\n') | Some(b'\r')) {
        b = &b[..b.len() - 1];
    }
    let Ok(text) = std::str::from_utf8(b) else {
        return;
    };
    if !text.contains("\"assistant\"") {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return;
    };
    if v.get("type").and_then(Value::as_str) != Some("message") {
        return;
    }
    let msg = v.get("message");
    if msg.and_then(|m| m.get("role")).and_then(Value::as_str) != Some("assistant") {
        return;
    }
    let Some(ts) = v
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
    else {
        return;
    };
    let model = msg
        .and_then(|m| m.get("modelId"))
        .and_then(Value::as_str)
        .map(str::to_string);
    out.push((ts.timestamp_millis(), model));
}

/// (ts_ms, message modelId) pairs for assistant messages, in file order.
/// `Some(vec![])` when the jsonl does not exist; `None` when it exists
/// but a read fails — a transient state the caller must not cache.
/// Lines are accumulated byte-wise and capped at SESSION_LINE_CAP, so a
/// transcript-dump line is consumed without ever being buffered whole.
fn assistant_messages(jsonl: &Path) -> Option<Vec<(i64, Option<String>)>> {
    let file = match std::fs::File::open(jsonl) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(_) => return None,
    };
    let mut out = Vec::new();
    let mut line: Vec<u8> = Vec::new();
    let mut oversized = false;
    let mut reader = std::io::BufReader::new(file);
    use std::io::BufRead;
    loop {
        let (take, eol) = {
            let buf = match reader.fill_buf() {
                Ok(b) => b,
                Err(_) => return None,
            };
            if buf.is_empty() {
                // EOF: flush an unterminated final line, then stop.
                if !oversized && !line.is_empty() {
                    push_message_line(&line, &mut out);
                }
                break;
            }
            let nl = buf.iter().position(|&b| b == b'\n');
            let take = nl.map(|i| i + 1).unwrap_or(buf.len());
            if !oversized && line.len() + take <= SESSION_LINE_CAP {
                line.extend_from_slice(&buf[..take]);
            } else {
                oversized = true;
            }
            (take, nl.is_some())
        };
        reader.consume(take);
        if eol {
            if !oversized {
                push_message_line(&line, &mut out);
            }
            line.clear();
            oversized = false;
        }
    }
    Some(out)
}

/// Decode one session into dated per-message shares of its tokenUsage.
/// A token-carrying session with no assistant messages attributes
/// everything to the settings file's own mtime. `None` marks a transient
/// read failure (a mid-write settings file, an unreadable jsonl) the
/// caller must not cache; `Some(vec![])` is a genuine zero-token session.
fn session_events(settings: &Path, settings_mtime: SystemTime) -> Option<Vec<DroidSessionEvent>> {
    let Ok(raw) = super::read_small_text(settings, SESSION_SETTINGS_CAP, "Droid session") else {
        return None;
    };
    let Ok(doc) = serde_json::from_str::<Value>(&raw) else {
        return None;
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
        return Some(Vec::new());
    }
    let settings_model = doc.get("model").and_then(Value::as_str).unwrap_or_default();
    let msgs = assistant_messages(&session_jsonl(settings))?;
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
        return Some(vec![event(ts_ms, None, 1.0)]);
    }
    let n = msgs.len() as f64;
    Some(msgs.into_iter().map(|(ts, m)| event(ts, m, n)).collect())
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
            if let Some(events) = session_events(&settings, s.0) {
                entry.2 = events;
                entry.0 = s;
                entry.1 = j;
            }
            // None is a transient read failure: keep the previous events
            // AND stamps so the next scan retries instead of caching it.
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

    // --- login refresh ---------------------------------------------------

    /// Serializes the tests that point FACTORY_HOME_OVERRIDE at a fixture
    /// dir — same pattern as dsh.rs's ENV_LOCK.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn factory_fixture(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pane-droid-refresh-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The decrypted doc shaped like the real file (4 top-level keys),
    /// written through the keyfile variant so no Credential Manager is
    /// involved. Returns the live file path and its raw line.
    fn write_factory_login(dir: &Path, doc: &Value) -> (PathBuf, String) {
        std::fs::write(dir.join("auth.v2.key"), B64.encode(TEST_KEY)).unwrap();
        let line = auth_line(TEST_KEY, TEST_IV, &serde_json::to_string(doc).unwrap());
        let live = write_auth(dir, "auth.v2.file", &line);
        (live, line)
    }

    #[test]
    fn refreshed_doc_swaps_only_the_token_pair() {
        let old = json!({
            "access_token": "at-old", "refresh_token": "rt-old",
            "active_organization_id": "org-7",
            "whoami": {"id": "u-1", "email": "a@b.c"},
            "some_future_key": {"nested": [1, 2]},
        });
        let new = refreshed_doc(&old, "at-new", "rt-new").unwrap();
        assert_eq!(new["access_token"], "at-new");
        assert_eq!(new["refresh_token"], "rt-new");
        assert_eq!(new["active_organization_id"], "org-7");
        assert_eq!(new["whoami"], json!({"id": "u-1", "email": "a@b.c"}));
        assert_eq!(new["some_future_key"], json!({"nested": [1, 2]}));
        // No top-level refresh_token → nothing to rotate.
        assert!(refreshed_doc(&json!({"access_token": "a"}), "x", "y").is_none());
    }

    #[test]
    fn encrypt_auth_line_round_trips_through_the_reader() {
        let doc = json!({"access_token": "a", "refresh_token": "r"});
        let line = encrypt_auth_line(TEST_KEY, &doc).unwrap();
        assert!(!line.ends_with('\n'));
        let parts: Vec<&str> = line.split(':').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(B64.decode(parts[0]).unwrap().len(), 16); // IV
        assert_eq!(B64.decode(parts[1]).unwrap().len(), 16); // tag
        let dir = std::env::temp_dir().join(format!("pane-droid-enc-{}", std::process::id()));
        let path = write_auth(&dir, "auth.v2.file", &line);
        let plain = decrypt_auth_file(&path, TEST_KEY).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&plain).unwrap(), doc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workos_client_id_prefers_the_token_claim() {
        let with = unsigned_jwt(json!({"client_id": "client_01TESTXYZ"}));
        assert_eq!(workos_client_id(&with), "client_01TESTXYZ");
        let other = unsigned_jwt(json!({"client_id": "not-a-client-id"}));
        assert_eq!(workos_client_id(&other), WORKOS_CLIENT_ID);
        let none = unsigned_jwt(json!({"sub": "u-1"}));
        assert_eq!(workos_client_id(&none), WORKOS_CLIENT_ID);
    }

    #[test]
    fn still_fresh_gates_on_the_refresh_window() {
        let now = 1_800_000_000i64;
        assert!(still_fresh(&unsigned_jwt(json!({"exp": now + 3_600})), now));
        // Inside the 5-minute lookahead → refresh.
        assert!(!still_fresh(&unsigned_jwt(json!({"exp": now + 60})), now));
        assert!(!still_fresh(&unsigned_jwt(json!({"exp": now - 60})), now));
        // No exp claim can't be proven stale → fresh.
        assert!(still_fresh(&unsigned_jwt(json!({"sub": "u-1"})), now));
    }

    #[test]
    fn droid_running_in_matches_only_the_exact_image() {
        assert!(droid_running_in(["explorer.exe", "Droid.EXE"].into_iter()));
        assert!(!droid_running_in(["droidx.exe", "pane.exe"].into_iter()));
        assert!(!droid_running_in(std::iter::empty()));
    }

    #[test]
    fn write_lock_blocks_only_while_fresh() {
        let now = SystemTime::now();
        assert!(!write_lock_blocks(None, now));
        assert!(write_lock_blocks(
            Some(now - std::time::Duration::from_secs(30)),
            now
        ));
        assert!(!write_lock_blocks(
            Some(now - std::time::Duration::from_secs(300)),
            now
        ));
    }

    #[test]
    fn refresh_backoff_permanent_is_per_token_and_transient_thirty_min() {
        let mut b = RefreshBackoff { transient_until_ms: 0, rejected_fp: None };
        let now = 1_000_000i64;
        assert!(backoff_allows(&b, "fp-a", now));
        // Permanent rejection: same fingerprint never retried, but a new
        // refresh token (droid re-login) proceeds right away.
        backoff_record(&mut b, Some("fp-a"), now);
        assert!(!backoff_allows(&b, "fp-a", now + 24 * 3_600_000));
        assert!(backoff_allows(&b, "fp-b", now));
        // Transient failure: everything waits 30 min.
        let mut t = RefreshBackoff { transient_until_ms: 0, rejected_fp: None };
        backoff_record(&mut t, None, now);
        assert!(!backoff_allows(&t, "fp-a", now + 29 * 60_000));
        assert!(backoff_allows(&t, "fp-a", now + 31 * 60_000));
    }

    /// Whole write-back path minus the HTTP call: prepare off a real
    /// fixture dir, then commit with the stubbed rotated pair.
    #[test]
    fn refresh_commit_writes_the_pair_and_keeps_a_backup() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = factory_fixture("commit");
        let old_access = unsigned_jwt(json!({
            "exp": 1_700_000_000i64, "id": "acct-1",
            "client_id": "client_01TEST000000000000000000"
        }));
        let old_doc = json!({
            "access_token": old_access, "refresh_token": "rt-old",
            "active_organization_id": "org-7",
            "whoami": {"id": "u-1", "email": "a@b.c"},
        });
        let (live, old_line) = write_factory_login(&home, &old_doc);
        let prev = std::env::var("FACTORY_HOME_OVERRIDE").ok();
        std::env::set_var("FACTORY_HOME_OVERRIDE", &home);

        let prep = prepare_refresh(&home, false, 1_000_000).unwrap();
        assert_eq!(prep.client_id, "client_01TEST000000000000000000");
        let new_access = unsigned_jwt(json!({"exp": 4_000_000_000i64, "id": "acct-1"}));
        let got = commit_refresh(prep, &new_access, "rt-new", false);
        assert_eq!(got.as_deref(), Some(new_access.as_str()));

        // The real read path now sees the rotated pair.
        let login = read_login().unwrap().unwrap();
        assert_eq!(login.access_token, new_access);
        // `.pane-bak` holds the old line; no tmp is left behind.
        assert_eq!(
            std::fs::read_to_string(home.join("auth.v2.file.pane-bak")).unwrap(),
            old_line
        );
        assert!(!home.join("auth.v2.file.pane-tmp").exists());
        // Other keys survived untouched.
        let plain = decrypt_auth_file(&live, TEST_KEY).unwrap();
        let doc: Value = serde_json::from_slice(&plain).unwrap();
        assert_eq!(doc["refresh_token"], "rt-new");
        assert_eq!(doc["active_organization_id"], "org-7");
        assert_eq!(doc["whoami"], json!({"id": "u-1", "email": "a@b.c"}));

        match prev {
            Some(v) => std::env::set_var("FACTORY_HOME_OVERRIDE", v),
            None => std::env::remove_var("FACTORY_HOME_OVERRIDE"),
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A droid write between read and commit must abort the commit and
    /// leave whatever it wrote alone.
    #[test]
    fn refresh_commit_aborts_when_the_live_file_changed() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = factory_fixture("race");
        let old_doc = json!({
            "access_token": "at-old", "refresh_token": "rt-old",
            "active_organization_id": "org-7",
            "whoami": {"id": "u-1"},
        });
        let (live, _old_line) = write_factory_login(&home, &old_doc);
        let prev = std::env::var("FACTORY_HOME_OVERRIDE").ok();
        std::env::set_var("FACTORY_HOME_OVERRIDE", &home);

        let prep = prepare_refresh(&home, false, 1_000_000).unwrap();
        std::fs::write(&live, "droid:wrote:this").unwrap();
        let got = commit_refresh(prep, "at-new", "rt-new", false);
        assert!(got.is_none());
        // Live file untouched; the staged tmp is gone.
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "droid:wrote:this");
        assert!(!home.join("auth.v2.file.pane-tmp").exists());

        match prev {
            Some(v) => std::env::set_var("FACTORY_HOME_OVERRIDE", v),
            None => std::env::remove_var("FACTORY_HOME_OVERRIDE"),
        }
        let _ = std::fs::remove_dir_all(&home);
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

    #[test]
    fn oversized_jsonl_line_is_skipped_and_the_rest_survives() {
        let home = session_dir("bigline");
        let _ = std::fs::remove_dir_all(&home);
        let dir = home.join("sessions").join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("big.settings.json"),
            serde_json::to_string(
                &json!({"model": "gpt-6-sol", "tokenUsage": usage(10.0, 5.0, 0.0, 0.0, 0.0)}),
            )
            .unwrap(),
        )
        .unwrap();
        // A transcript-dump line past the cap that would parse as an
        // assistant message if it were ever buffered whole, then a real one.
        let mut jsonl =
            b"{\"type\":\"message\",\"timestamp\":\"2026-10-01T09:00:00Z\",\"message\":{\"role\":\"assistant\",\"modelId\":\"gpt-6-sol\"},\"pad\":\"".to_vec();
        jsonl.extend(std::iter::repeat_n(b'x', SESSION_LINE_CAP));
        jsonl.extend_from_slice(b"\"}\n{\"type\":\"message\",\"timestamp\":\"2026-10-01T10:00:00Z\",\"message\":{\"role\":\"assistant\",\"modelId\":\"gpt-6-astra\"}}");
        std::fs::write(dir.join("big.jsonl"), &jsonl).unwrap();
        let mut cache = SessionCache::new();
        let events = collect_from(&home, SystemTime::UNIX_EPOCH, &mut cache);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].model, "gpt-6-astra");
        assert_eq!(events[0].input, 10.0);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn transient_read_failure_keeps_cached_events() {
        let home = session_dir("transient");
        let _ = std::fs::remove_dir_all(&home);
        write_session(
            &home,
            "proj",
            "t1",
            &json!({"model": "gpt-6-sol", "tokenUsage": usage(100.0, 50.0, 0.0, 0.0, 0.0)}),
            &["{\"type\":\"message\",\"timestamp\":\"2026-10-01T10:00:00Z\",\"message\":{\"role\":\"assistant\",\"modelId\":\"gpt-6-sol\"}}"],
        );
        let mut cache = SessionCache::new();
        let first = collect_from(&home, SystemTime::UNIX_EPOCH, &mut cache);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].input, 100.0);

        // Mid-write settings: unparsable JSON with a changed stamp. The
        // last-good events are served while the failure stays uncached.
        let settings_path = home.join("sessions/proj/t1.settings.json");
        std::fs::write(&settings_path, "{not valid json at all").unwrap();
        let second = collect_from(&home, SystemTime::UNIX_EPOCH, &mut cache);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].input, 100.0);

        // Once the file parses again the new contents are picked up.
        write_session(
            &home,
            "proj",
            "t1",
            &json!({"model": "gpt-6-astra", "tokenUsage": usage(40.0, 10.0, 0.0, 0.0, 0.0)}),
            &["{\"type\":\"message\",\"timestamp\":\"2026-10-02T10:00:00Z\",\"message\":{\"role\":\"assistant\",\"modelId\":\"gpt-6-astra\"}}"],
        );
        let third = collect_from(&home, SystemTime::UNIX_EPOCH, &mut cache);
        assert_eq!(third.len(), 1);
        assert_eq!(third[0].input, 40.0);
        assert_eq!(third[0].model, "gpt-6-astra");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn missing_jsonl_falls_back_to_settings_mtime() {
        let home = session_dir("nojsonl");
        let _ = std::fs::remove_dir_all(&home);
        let dir = home.join("sessions").join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("solo.settings.json"),
            serde_json::to_string(
                &json!({"model": "claude-fable-5.1", "tokenUsage": usage(20.0, 5.0, 0.0, 0.0, 0.0)}),
            )
            .unwrap(),
        )
        .unwrap();
        // No .jsonl sibling at all.
        let mut cache = SessionCache::new();
        let events = collect_from(&home, SystemTime::UNIX_EPOCH, &mut cache);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].model, "claude-fable-5.1");
        let st_mtime = file_stamp(&dir.join("solo.settings.json"))
            .0
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        assert_eq!(events[0].ts_ms, st_mtime);
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
