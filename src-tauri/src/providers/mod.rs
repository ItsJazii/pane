pub mod aihubmix;
pub mod antigravity;
pub mod claude;
pub mod codebuff;
pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod deepseek;
pub mod devin;
pub mod elevenlabs;
pub mod grok;
pub mod hermes;
pub mod kilo;
pub mod kimi;
pub mod minimax;
pub mod moonshot;
pub mod ollama;
pub mod onenewapi;
pub mod opencode;
pub mod openrouter;
pub mod qwen;
pub mod sub2api;
pub mod zai;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// One row inside a provider card, e.g. "Session ▓▓▓░░ 43% left · Resets in 2h".
/// `resets_at` (epoch ms) + `period_ms` are the structured facts the pace
/// engine needs; the UI formats countdowns and projections from them.
#[derive(Serialize, Deserialize, Clone)]
pub struct Metric {
    pub label: String,
    pub kind: String, // "progress" | "text"
    pub used_percent: Option<f64>,
    pub detail: Option<String>,
    pub value: Option<String>,
    pub resets_at: Option<i64>,
    pub period_ms: Option<i64>,
}

impl Metric {
    pub fn progress(label: &str, used_percent: f64, detail: Option<String>) -> Self {
        Self {
            label: label.into(),
            kind: "progress".into(),
            used_percent: Some(used_percent),
            detail,
            value: None,
            resets_at: None,
            period_ms: None,
        }
    }

    #[allow(dead_code)]
    pub fn text(label: &str, value: String) -> Self {
        Self {
            label: label.into(),
            kind: "text".into(),
            used_percent: None,
            detail: None,
            value: Some(value),
            resets_at: None,
            period_ms: None,
        }
    }

    pub fn with_reset(mut self, resets_at: Option<i64>, period_ms: Option<i64>) -> Self {
        self.resets_at = resets_at;
        self.period_ms = period_ms;
        self
    }
}

/// Everything one provider reports back after a refresh. `stale` marks a
/// snapshot that is actually the last good fetch, shown because the newest
/// attempt failed transiently (`warning` carries that error). `fetched_at`
/// is when this data was last successfully fetched (epoch ms) — it rides
/// along so a restored snapshot can't pose as a fresh success downstream
/// (local HTTP API, tray, notifications). `None` means "unknown", which
/// only happens before the first success.
#[derive(Serialize, Deserialize, Clone)]
pub struct Snapshot {
    pub id: String,
    pub name: String,
    pub plan: Option<String>,
    pub status: String, // "ok" | "no_credentials" | "error"
    pub error: Option<String>,
    pub metrics: Vec<Metric>,
    pub stale: bool,
    pub warning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard_url: Option<String>,
}

impl Snapshot {
    pub fn ok(id: &str, name: &str, plan: Option<String>, metrics: Vec<Metric>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            plan,
            status: "ok".into(),
            error: None,
            metrics,
            stale: false,
            warning: None,
            fetched_at: None,
            dashboard_url: None,
        }
    }

    pub fn no_credentials(id: &str, name: &str, hint: &str) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            plan: None,
            status: "no_credentials".into(),
            error: Some(hint.into()),
            metrics: vec![],
            stale: false,
            warning: None,
            fetched_at: None,
            dashboard_url: None,
        }
    }

    pub fn error(id: &str, name: &str, message: String) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            plan: None,
            status: "error".into(),
            error: Some(message),
            metrics: vec![],
            stale: false,
            warning: None,
            fetched_at: None,
            dashboard_url: None,
        }
    }
}

/// Optional outbound proxy from config.json `proxy: { enabled, url }`.
/// Loaded once per app run (Mac parity — a change needs a restart) and never
/// applied to loopback, so the local Antigravity/HTTP-API traffic stays direct.
fn proxy_url() -> Option<&'static str> {
    static PROXY: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    PROXY
        .get_or_init(|| {
            let cfg: serde_json::Value = std::fs::read_to_string(config_dir().join("config.json"))
                .ok()
                .and_then(|raw| serde_json::from_str(raw.trim_start_matches('\u{feff}')).ok())?;
            let proxy = cfg.get("proxy")?;
            if !proxy
                .get("enabled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                return None;
            }
            let url = proxy.get("url")?.as_str()?.trim().to_string();
            let valid = ["http://", "https://", "socks5://"]
                .iter()
                .any(|s| url.starts_with(s));
            if url.is_empty() || !valid {
                return None;
            }
            Some(url)
        })
        .as_deref()
}

fn http_builder() -> reqwest::ClientBuilder {
    let mut builder = reqwest::Client::builder()
        .user_agent("Pane-Windows/0.3")
        .timeout(std::time::Duration::from_secs(20))
        // At boot the network is often still coming up; without a connect
        // cap every request rides the full 20 s, and the UI's first paint
        // waits on the slowest provider chain. Failing to connect in 5 s
        // is a dead network — fail fast, serve the cached snapshot.
        .connect_timeout(std::time::Duration::from_secs(5));
    if let Some(url) = proxy_url() {
        if let Ok(proxy) = reqwest::Proxy::all(url) {
            let proxy = proxy.no_proxy(reqwest::NoProxy::from_string("localhost,127.0.0.1,::1"));
            builder = builder.proxy(proxy);
        }
    }
    builder
}

pub fn http() -> reqwest::Client {
    http_builder().build().expect("failed to build http client")
}

/// Same client as [`http`] but never follows redirects. One/New API status
/// and billing calls must not be bounced onto another origin.
pub fn http_no_redirect() -> reqwest::Client {
    http_builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build http client")
}

/// JSON bodies from vendor APIs are tiny (quota + token responses). Cap
/// before parse so a huge payload can't stall a refresh or blow RAM —
/// same idea as the share-card decode bound.
/// Bounded body reader shared by every JSON vendor call: downloads in
/// chunks and stops the moment the running total exceeds `max_bytes`.
/// Without Content-Length there is no header to trust, so the cap has to
/// be enforced WHILE downloading — `resp.bytes()` buffers the whole
/// payload in RAM first and only then notices. Content-Length, when
/// present, is only an early-reject shortcut. Transport errors are
/// stripped of their URL: vendor origins (One/New API panels especially)
/// stay out of error text.
pub(crate) async fn read_body_bounded(
    resp: &mut reqwest::Response,
    max_bytes: usize,
    what: &str,
) -> Result<Vec<u8>, String> {
    if resp.content_length().is_some_and(|n| n > max_bytes as u64) {
        return Err(format!("{what}: response too large"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("{what}: {}", e.without_url()))?
    {
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err(format!("{what}: response too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// JSON bodies from vendor APIs are tiny (quota + token responses). Cap
/// before parse so a huge payload can't stall a refresh or blow RAM —
/// same idea as the share-card decode bound.
pub(crate) async fn json_body(
    resp: reqwest::Response,
    max_bytes: usize,
    what: &str,
) -> Result<serde_json::Value, String> {
    let mut resp = resp;
    let bytes = read_body_bounded(&mut resp, max_bytes, what).await?;
    serde_json::from_slice(&bytes).map_err(|e| format!("{what} parse: {e}"))
}

pub(crate) fn read_small_text(
    path: &std::path::Path,
    max_bytes: u64,
    what: &str,
) -> Result<String, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("read {what}: {e}"))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(format!("{what} is not a regular file"));
    }
    // Read through a bounded reader — a file that grows or is swapped after
    // the metadata check still can't exceed the cap.
    let file = std::fs::File::open(path).map_err(|e| format!("read {what}: {e}"))?;
    let mut limited = std::io::Read::take(file, max_bytes + 1);
    let mut text = String::new();
    std::io::Read::read_to_string(&mut limited, &mut text)
        .map_err(|e| format!("read {what}: {e}"))?;
    if text.len() as u64 > max_bytes {
        return Err(format!("{what} is unexpectedly large — not reading it"));
    }
    Ok(text)
}

/// Where Pane keeps its own settings, e.g. saved API keys:
/// C:\Users\you\AppData\Roaming\Pane
///
/// The app shipped as "OpenUsage" before the rename — on first call, an
/// existing %APPDATA%\OpenUsage is moved over so nobody loses their config,
/// keys, or caches. If the move fails but the old dir is usable, keep using
/// the old dir rather than silently starting fresh.
pub fn config_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let base = dirs::config_dir().unwrap_or_default();
        let new = base.join("Pane");
        let old = base.join("OpenUsage");
        if !new.exists() && old.exists() {
            let _ = std::fs::rename(&old, &new);
            if !new.exists() {
                return old;
            }
        }
        new
    })
    .clone()
}

/// Reads a generic credential's blob from Windows Credential Manager.
pub fn read_windows_credential(target: &str) -> Option<Vec<u8>> {
    use windows::core::PCWSTR;
    use windows::Win32::Security::Credentials::{
        CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC,
    };
    let wide: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    let mut pcred: *mut CREDENTIALW = std::ptr::null_mut();
    unsafe {
        if CredReadW(PCWSTR(wide.as_ptr()), CRED_TYPE_GENERIC, None, &mut pcred).is_err() {
            return None;
        }
        let cred = &*pcred;
        let blob =
            std::slice::from_raw_parts(cred.CredentialBlob, cred.CredentialBlobSize as usize)
                .to_vec();
        CredFree(pcred as *mut std::ffi::c_void);
        Some(blob)
    }
}

/// Credential blob → text: UTF-8 or UTF-16 LE, unwrapping go-keyring's
/// `go-keyring-base64:` prefix (used by Go CLIs like gh and Antigravity).
pub fn credential_string(target: &str) -> Option<String> {
    let blob = read_windows_credential(target)?;
    let text = String::from_utf8(blob.clone()).ok().or_else(|| {
        if blob.len() % 2 == 0 {
            let utf16: Vec<u16> = blob
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16(&utf16).ok()
        } else {
            None
        }
    })?;
    let text = text.trim().trim_matches('\0').to_string();
    if let Some(b64) = text.strip_prefix("go-keyring-base64:") {
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .ok()?;
        return String::from_utf8(decoded).ok();
    }
    Some(text)
}

/// Percent-used meter for pay-as-you-go balances. These APIs report only
/// what's left — never "of how much" — so Pane remembers the highest
/// balance it has ever seen per provider (a top-up raises it automatically)
/// and meters usage against that high-water mark. Persisted so restarts
/// keep the story. As a progress row it also feeds the notification rules
/// ("Almost Out" fires under 10% remaining) like every other meter.
pub fn credit_meter(provider: &str, sign: &str, balance: f64) -> Option<Metric> {
    credit_meter_labeled(provider, sign, balance, "Credits used", "")
}

/// Shared with forget_credit_baselines_in so a key rotation cannot race
/// a refresh that is raising the high-water mark.
static CREDIT_BASELINE_LOCK: Mutex<()> = Mutex::new(());
static CREDIT_BASELINE_GENERATIONS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
static CREDIT_METER_INFLIGHT: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();

fn credit_baseline_generations() -> &'static Mutex<HashMap<String, u64>> {
    CREDIT_BASELINE_GENERATIONS.get_or_init(Default::default)
}

/// Generation last bumped by `forget_credit_baselines_in`. A fetch that
/// started before that bump must not persist its leftover balance.
pub fn credit_baseline_generation(id: &str) -> u64 {
    credit_baseline_generations()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id)
        .copied()
        .unwrap_or(0)
}

fn bump_credit_baseline_generations(ids: &[String]) {
    let mut generations = credit_baseline_generations()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for id in ids {
        *generations.entry(id.clone()).or_default() += 1;
    }
}

/// Remember the baseline generation a live fetch started under. Late
/// `credit_meter_labeled` calls then refuse to rewrite a rotated key's
/// high-water mark. Unbind from a drop guard so panics cannot leak it.
pub fn bind_credit_meter_generation(id: &str, gen: u64) {
    CREDIT_METER_INFLIGHT
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.to_string(), gen);
}

pub fn unbind_credit_meter_generation(id: &str) {
    CREDIT_METER_INFLIGHT
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(id);
}

fn expected_credit_meter_generation(provider: &str) -> u64 {
    CREDIT_METER_INFLIGHT
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(provider)
        .copied()
        .unwrap_or_else(|| credit_baseline_generation(provider))
}

/// credit_meter with a caller-chosen row label and caption suffix —
/// purchased-credit pools (Codex Extra credits, Devin's extra balance)
/// meter identically but shouldn't all be called "Credits used", and some
/// carry an extra unit in the caption ("· N credits").

pub fn credit_meter_labeled(
    provider: &str,
    sign: &str,
    balance: f64,
    label: &str,
    caption_suffix: &str,
) -> Option<Metric> {
    credit_meter_labeled_in(
        &config_dir(),
        provider,
        sign,
        balance,
        label,
        caption_suffix,
    )
}

pub fn credit_meter_labeled_in(
    dir: &Path,
    provider: &str,
    sign: &str,
    balance: f64,
    label: &str,
    caption_suffix: &str,
) -> Option<Metric> {
    if !balance.is_finite() || balance < 0.0 {
        return None;
    }
    let expected_gen = expected_credit_meter_generation(provider);
    // Providers refresh concurrently and this is a read-modify-write on a
    // shared file — serialize it, or one card's just-raised high-water
    // mark can be overwritten by another's stale copy.
    let _guard = CREDIT_BASELINE_LOCK.lock();
    let stale = credit_baseline_generation(provider) != expected_gen;
    let path = dir.join("credit_baselines.json");
    let mut doc: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    let high = doc
        .get(provider)
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);
    // A late result from a rotated key must not recreate the deleted
    // baseline from the old account's leftover balance.
    if !stale && balance > high {
        doc[provider] = serde_json::Value::from(balance);
        let _ = std::fs::write(
            &path,
            serde_json::to_string_pretty(&doc).unwrap_or_default(),
        );
    }
    let high = if stale { high } else { high.max(balance) };
    if high <= 0.0 {
        return None;
    }
    let used = ((1.0 - balance / high) * 100.0).clamp(0.0, 100.0);
    Some(Metric::progress(
        label,
        used,
        Some(format!(
            "{sign}{balance:.2} of {sign}{high:.2} left{caption_suffix}"
        )),
    ))
}

/// Drop persisted high-water marks for the given provider ids. A rotated
/// API key must not inherit the previous account's credit baseline, or
/// the new balance is compared against the old pot until it exceeds it.
/// Write failures are returned so a key save cannot report success while
/// the old pot is still on disk.
pub fn forget_credit_baselines_in(dir: &Path, ids: &[String]) -> Result<(), String> {
    if ids.is_empty() {
        return Ok(());
    }
    let _guard = CREDIT_BASELINE_LOCK.lock();
    bump_credit_baseline_generations(ids);
    let path = dir.join("credit_baselines.json");
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("read credit baselines: {e}")),
    };
    let mut doc: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| format!("parse credit baselines: {e}"))?;
    let Some(obj) = doc.as_object_mut() else {
        return Ok(());
    };
    let mut changed = false;
    for id in ids {
        changed |= obj.remove(id).is_some();
    }
    if changed {
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&doc).unwrap_or_default(),
        )
        .map_err(|e| format!("write credit baselines: {e}"))?;
    }
    Ok(())
}

pub fn credit_baselines_contain(dir: &Path, ids: &[String]) -> bool {
    if ids.is_empty() {
        return false;
    }
    let Ok(raw) = std::fs::read_to_string(dir.join("credit_baselines.json")) else {
        return false;
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    ids.iter().any(|id| doc.get(id).is_some())
}

/// Candidate roots where a second account's CLI config dir may live:
/// dot-folders in the home directory plus dirs under ~/.config — the
/// places CLAUDE_CONFIG_DIR / CODEX_HOME setups conventionally point.
/// Shared by every provider family that supports multi-account discovery.
pub(crate) fn account_scan_roots() -> Vec<std::path::PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let mut roots = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&home) {
        for e in entries.flatten() {
            let p = e.path();
            let dotted = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'));
            if dotted && p.is_dir() {
                roots.push(p);
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(home.join(".config")) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                roots.push(p);
            }
        }
    }
    roots
}

/// True when Customize has this provider switched off. Disabled providers
/// must not make network calls — including a folded-in wallet fetch that
/// lives on another card (Kimi Code's Moonshot API bar).
pub fn provider_disabled(id: &str) -> bool {
    let Ok(raw) = std::fs::read_to_string(config_dir().join("config.json")) else {
        return false;
    };
    let Ok(cfg) = serde_json::from_str::<serde_json::Value>(raw.trim_start_matches('\u{feff}'))
    else {
        return false;
    };
    cfg.get("disabled")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(id)))
}

/// API key lookup: our saved config file first, then environment variables.
pub fn stored_api_key(provider: &str, env_vars: &[&str]) -> Option<String> {
    let path = config_dir().join(format!("{provider}.json"));
    if let Ok(raw) = std::fs::read_to_string(&path) {
        if let Ok(doc) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(key) = doc.get("apiKey").and_then(serde_json::Value::as_str) {
                let key = key.trim();
                if !key.is_empty() {
                    return Some(key.to_string());
                }
            }
        }
    }
    for var in env_vars {
        if let Ok(key) = std::env::var(var) {
            let key = key.trim().to_string();
            if !key.is_empty() {
                return Some(key);
            }
        }
    }
    None
}

/// Hard cap for any leftover temp copy path. Multi-GB ledgers (Devin) must
/// never be cloned onto C:.
pub(crate) const MAX_TEMP_SQLITE_BYTES: u64 = 64 * 1024 * 1024;

/// Row cap for full-table ledger reads (Hermes, MiniMax, OpenCode). Real
/// ledgers never get near it; a runaway vendor db must not materialize an
/// unbounded Vec.
pub(crate) const MAX_LEDGER_ROWS: u64 = 2_000_000;

// Test-only: production callers enforce the cap on the copy as it is
// written — a size pre-check races the copy. Minimax's test-only snapshot
// helper still uses this.
#[cfg(test)]
pub(crate) fn temp_sqlite_copy_allowed(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.len())
        .unwrap_or(u64::MAX)
        <= MAX_TEMP_SQLITE_BYTES
}

pub(crate) fn open_readonly_sqlite(
    path: &std::path::Path,
) -> Result<rusqlite::Connection, String> {
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| format!("open live db: {e}"))?;
    conn.busy_timeout(std::time::Duration::from_millis(250))
        .map_err(|e| format!("busy timeout: {e}"))?;
    Ok(conn)
}

/// Delete a SQLite file and its `-wal` / `-shm` sidecars. Call this before
/// opening a reused temp destination — removing only the `.db` leaves the
/// journal, and the next backup appends another full copy to it.
#[cfg(test)]
pub(crate) fn remove_sqlite_files(db_path: &std::path::Path) {
    let _ = std::fs::remove_file(db_path);
    let mut wal = db_path.as_os_str().to_os_string();
    wal.push("-wal");
    let _ = std::fs::remove_file(&wal);
    let mut shm = db_path.as_os_str().to_os_string();
    shm.push("-shm");
    let _ = std::fs::remove_file(&shm);
    let mut journal = db_path.as_os_str().to_os_string();
    journal.push("-journal");
    let _ = std::fs::remove_file(&journal);
}

/// Drop leftover Pane temp snapshots in `%TEMP%` (`pane-devin-*.db` and
/// friends). A crashed or overlapping spend scan used to leave multi-GB
/// WAL journals on C:.
pub fn sweep_temp_sqlite_copies() {
    for prefix in ["pane-devin-", "pane-minimax-", "pane-hermes-"] {
        sweep_temp_sqlite_prefix(prefix);
    }
    sweep_temp_prefix_ext("openusage-cursor-", &[".vscdb", ".vscdb-wal", ".vscdb-shm"]);
    sweep_opencode_scratch();
}

fn is_pane_temp_sqlite(name: &str, prefix: &str) -> bool {
    let Some(rest) = name.strip_prefix(prefix) else {
        return false;
    };
    let stem = rest
        .strip_suffix(".db-wal")
        .or_else(|| rest.strip_suffix(".db-shm"))
        .or_else(|| rest.strip_suffix(".db-journal"))
        .or_else(|| rest.strip_suffix(".db"));
    let Some(stem) = stem else {
        return false;
    };
    !stem.is_empty()
        && stem.chars().all(|c| c.is_ascii_digit() || c == '-')
        && stem.chars().any(|c| c.is_ascii_digit())
}

pub(crate) fn sweep_temp_sqlite_prefix(prefix: &str) {
    sweep_temp_dir(std::env::temp_dir(), |name| is_pane_temp_sqlite(name, prefix));
}

fn sweep_temp_prefix_ext(prefix: &str, suffixes: &[&str]) {
    sweep_temp_dir(std::env::temp_dir(), |name| {
        name.starts_with(prefix) && suffixes.iter().any(|s| name.ends_with(s))
    });
}

fn sweep_opencode_scratch() {
    let dir = config_dir().join("tmp");
    sweep_temp_dir(dir, |name| name.starts_with("openusage-oc-"));
}

fn sweep_temp_dir(dir: std::path::PathBuf, keep: impl Fn(&str) -> bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for ent in entries.flatten() {
        let name = ent.file_name();
        let Some(name) = name.to_str() else { continue };
        if keep(name) {
            let _ = std::fs::remove_file(ent.path());
        }
    }
}

#[cfg(test)]
mod json_body_tests {
    use super::json_body;

    const MAX_BYTES: usize = 16;

    enum After {
        /// Send the payload, then close the socket.
        Close,
        /// Send the payload, then hold the connection open forever — a
        /// stream whose end never arrives.
        Hold,
    }

    /// One-shot mock vendor on a raw TCP socket: full control over
    /// headers, chunk boundaries, and whether the stream ever ends.
    fn serve_once(head: &str, body: &str, after: After) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let head = head.to_string();
        let body = body.to_string();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            use std::io::Read;
            let _ = stream.read(&mut buf);
            use std::io::Write;
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
            let _ = stream.flush();
            if let After::Close = after {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            } else {
                std::thread::park();
            }
        });
        addr
    }

    fn chunked(chunks: &[&str]) -> String {
        let mut body = String::new();
        for chunk in chunks {
            body.push_str(&format!("{:x}\r\n{chunk}\r\n", chunk.len()));
        }
        body.push_str("0\r\n\r\n");
        body
    }

    async fn get(addr: &str) -> reqwest::Response {
        reqwest::Client::new()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn chunked_body_over_limit_is_refused_without_waiting_for_the_end() {
        // No Content-Length; the cap must trip on the accumulated chunks,
        // not on the terminator that never arrives.
        let addr = serve_once(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
            &chunked(&["0123456789abcdef", "more"]),
            After::Hold,
        );
        let resp = tokio::time::timeout(std::time::Duration::from_secs(5), get(&addr))
            .await
            .expect("request");
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            json_body(resp, MAX_BYTES, "test"),
        )
        .await
        .expect("over-limit stream must be refused without waiting for its end");
        assert!(result.unwrap_err().contains("too large"));
    }

    #[tokio::test]
    async fn declared_oversized_content_length_is_rejected_up_front() {
        let addr = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 999\r\n\r\n",
            "",
            After::Hold,
        );
        let resp = get(&addr).await;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            json_body(resp, MAX_BYTES, "test"),
        )
        .await
        .expect("declared size must be rejected before any body arrives");
        assert!(result.unwrap_err().contains("too large"));
    }

    #[tokio::test]
    async fn body_exactly_at_the_limit_parses() {
        let mut body = r#"{"ok":true}"#.to_string();
        while body.len() < MAX_BYTES {
            body.push(' '); // trailing whitespace is legal JSON
        }
        assert_eq!(body.len(), MAX_BYTES);
        let addr = serve_once(
            &format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()),
            &body,
            After::Close,
        );
        let resp = get(&addr).await;
        let doc = json_body(resp, MAX_BYTES, "test").await.unwrap();
        assert_eq!(doc["ok"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn accumulation_across_chunks_enforces_the_limit() {
        // Two chunks of 12 bytes each: fine individually, over the 16-byte
        // cap together — the reader must add, not replace.
        let addr = serve_once(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
            &chunked(&["0123456789ab", "cdefghij"]),
            After::Close,
        );
        let resp = get(&addr).await;
        assert!(json_body(resp, MAX_BYTES, "test")
            .await
            .unwrap_err()
            .contains("too large"));
    }

    #[tokio::test]
    async fn stream_broken_mid_body_is_a_transport_error_without_the_origin() {
        // A truncated chunk frame: the body neither completes nor exceeds
        // the cap — the connection itself breaks.
        let addr = serve_once(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
            "5\r\nab",
            After::Close,
        );
        let resp = get(&addr).await;
        let error = json_body(resp, MAX_BYTES, "test").await.unwrap_err();
        assert!(!error.contains("too large"), "{error}");
        assert!(
            !error.contains(&addr),
            "origin must stay out of error text: {error}"
        );
    }

    #[tokio::test]
    async fn size_ok_but_invalid_json_is_a_parse_error() {
        let addr = serve_once(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
            &chunked(&["{not json"]),
            After::Close,
        );
        let resp = get(&addr).await;
        assert!(json_body(resp, MAX_BYTES, "test")
            .await
            .unwrap_err()
            .contains("parse"));
    }
}

#[cfg(test)]
mod sqlite_temp_tests {
    #[test]
    fn sweep_removes_devin_temp_journals() {
        let marker = std::env::temp_dir().join(format!(
            "pane-devin-{}-9.db-wal",
            std::process::id()
        ));
        std::fs::write(&marker, b"leftover").unwrap();
        assert!(marker.exists());
        super::sweep_temp_sqlite_prefix("pane-devin-");
        assert!(
            !marker.exists(),
            "sweep must delete leftover pane-devin journals"
        );
    }

    #[test]
    fn sweep_skips_unrelated_temp_names() {
        assert!(super::is_pane_temp_sqlite(
            "pane-devin-10568.db-wal",
            "pane-devin-"
        ));
        assert!(super::is_pane_temp_sqlite(
            "pane-minimax-10568-3.db",
            "pane-minimax-"
        ));
        assert!(super::is_pane_temp_sqlite(
            "pane-devin-16636.db-journal",
            "pane-devin-"
        ));
        assert!(!super::is_pane_temp_sqlite(
            "pane-hermes-narrow-test-1.db",
            "pane-hermes-"
        ));
        assert!(!super::is_pane_temp_sqlite("other-10568.db", "pane-devin-"));
    }

    #[test]
    fn huge_sqlite_files_are_never_copied() {
        assert!(super::temp_sqlite_copy_allowed(std::path::Path::new(".")));
        let huge = std::env::temp_dir().join(format!(
            "pane-size-cap-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        // Don't write 64MB; the helper treats a missing file as too large.
        assert!(!super::temp_sqlite_copy_allowed(&huge));
    }
}

#[cfg(test)]
mod credit_baseline_tests {
    use super::{
        bind_credit_meter_generation, credit_baseline_generation, credit_meter_labeled_in,
        forget_credit_baselines_in, unbind_credit_meter_generation,
    };
    use serde_json::Value;
    use std::path::PathBuf;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "pane-credit-{}-{stamp}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn read_baselines(dir: &std::path::Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(dir.join("credit_baselines.json")).unwrap())
            .unwrap()
    }

    #[test]
    fn late_credit_meter_does_not_restore_forgotten_baseline() {
        let tmp = TempDir::new();
        std::fs::write(
            tmp.0.join("credit_baselines.json"),
            r#"{"deepseek": 40.0}"#,
        )
        .unwrap();
        let started = credit_baseline_generation("deepseek");
        bind_credit_meter_generation("deepseek", started);
        forget_credit_baselines_in(&tmp.0, &["deepseek".into()]).unwrap();
        assert!(
            credit_baseline_generation("deepseek") > started,
            "forget must bump the generation a late fetch still holds"
        );
        let meter = credit_meter_labeled_in(&tmp.0, "deepseek", "$", 12.0, "Credits used", "");
        unbind_credit_meter_generation("deepseek");
        assert!(
            meter.is_none(),
            "a stale leftover balance must not become the new high-water"
        );
        let doc = read_baselines(&tmp.0);
        assert!(
            doc.get("deepseek").is_none(),
            "late credit_meter must not rewrite the deleted baseline"
        );
    }

    #[test]
    fn forget_credit_baselines_reports_unreadable_file() {
        let tmp = TempDir::new();
        std::fs::create_dir(tmp.0.join("credit_baselines.json")).unwrap();
        let err = forget_credit_baselines_in(&tmp.0, &["deepseek".into()]).unwrap_err();
        assert!(
            err.contains("credit baselines"),
            "baseline IO failures must reach the key-save caller: {err}"
        );
    }

    #[test]
    fn forget_credit_baselines_missing_file_is_ok() {
        let tmp = TempDir::new();
        forget_credit_baselines_in(&tmp.0, &["deepseek".into()]).unwrap();
    }
}
