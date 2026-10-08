//! Antigravity (Google's AI IDE). Mirrors the Mac app's three-step strategy:
//! 1. Talk to the IDE's local language server (found by scanning processes
//!    for `language_server*` / `agy` started with Antigravity flags).
//! 2. Fall back to Google's Cloud Code API with the OAuth token Antigravity
//!    keeps in Windows Credential Manager (`gemini:antigravity`).
//! 3. Report honestly when neither works.

use std::time::Duration;

use serde_json::{json, Value};

use super::{Metric, Snapshot};

const ID: &str = "antigravity";
const NAME: &str = "Antigravity";

const LS_SERVICE: &str = "exa.language_server_pb.LanguageServerService";
const CLOUD_BASES: [&str; 2] = [
    "https://daily-cloudcode-pa.googleapis.com",
    "https://cloudcode-pa.googleapis.com",
];
// Installed-app OAuth client — intentionally public, same values the IDE ships.
const GOOGLE_CLIENT_ID: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
const GOOGLE_CLIENT_SECRET: &str = "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";

pub async fn snapshot() -> Snapshot {
    // Process discovery + netstat are blocking child-process calls.
    let servers = tauri::async_runtime::spawn_blocking(discover_language_servers)
        .await
        .unwrap_or_default();

    for server in &servers {
        if let Some(snap) = try_language_server(server).await {
            return snap;
        }
    }

    match try_cloud().await {
        CloudResult::Ok(snap) => snap,
        CloudResult::AuthExpired => Snapshot::error(
            ID,
            NAME,
            "Antigravity sign-in expired. Open Antigravity to refresh it.".into(),
        ),
        CloudResult::Unavailable => Snapshot::error(
            ID,
            NAME,
            "Antigravity usage is temporarily unavailable. Try again shortly.".into(),
        ),
        CloudResult::NoCredentials => {
            if installed() {
                Snapshot::no_credentials(ID, NAME, "Start Antigravity once and try again.")
            } else {
                Snapshot::no_credentials(ID, NAME, "Antigravity not found on this PC.")
            }
        }
    }
}

fn installed() -> bool {
    let mut candidates = Vec::new();
    if let Ok(appdata) = std::env::var("APPDATA") {
        candidates.push(std::path::PathBuf::from(appdata).join("Antigravity"));
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        candidates.push(std::path::PathBuf::from(local).join("Programs").join("Antigravity"));
    }
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".antigravity"));
    }
    candidates.iter().any(|p| p.exists())
}

// ---------------------------------------------------------------------------
// Language-server discovery
// ---------------------------------------------------------------------------

struct LanguageServer {
    ports: Vec<u16>,
    extension_port: Option<u16>,
    csrf_token: String,
}

/// Runs a console command without flashing a window.
fn run_hidden(program: &str, args: &[&str]) -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `--flag value` or `--flag=value` from a whitespace-tokenized command line.
fn flag_value(tokens: &[&str], flag: &str) -> Option<String> {
    for (i, t) in tokens.iter().enumerate() {
        if let Some(v) = t.strip_prefix(&format!("{flag}=")) {
            return Some(v.trim_matches('"').to_string());
        }
        if *t == flag {
            return tokens.get(i + 1).map(|v| v.trim_matches('"').to_string());
        }
    }
    None
}

fn discover_language_servers() -> Vec<LanguageServer> {
    let script = "Get-CimInstance Win32_Process | Where-Object { $_.Name -match '^(language_server|agy)' } | Select-Object ProcessId, Name, CommandLine | ConvertTo-Json -Compress";
    let raw = match run_hidden("powershell", &["-NoProfile", "-NonInteractive", "-Command", script])
    {
        Some(r) if !r.trim().is_empty() => r,
        _ => return Vec::new(),
    };
    let parsed: Value = match serde_json::from_str(raw.trim()) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let procs: Vec<Value> = match parsed {
        Value::Array(a) => a,
        obj @ Value::Object(_) => vec![obj],
        _ => Vec::new(),
    };

    // netstat once, shared across processes: pid -> listening loopback ports.
    let netstat = run_hidden("netstat", &["-ano", "-p", "TCP"]).unwrap_or_default();

    let mut found = Vec::new();
    for p in procs {
        let cmdline = p.get("CommandLine").and_then(Value::as_str).unwrap_or("");
        let pid = p.get("ProcessId").and_then(Value::as_u64).unwrap_or(0) as u32;
        if cmdline.is_empty() || pid == 0 {
            continue;
        }
        let tokens: Vec<&str> = cmdline.split_whitespace().collect();

        // Only Antigravity's own language server — Windsurf ships the same
        // binary with a different --ide_name.
        let ide_name = flag_value(&tokens, "--ide_name")
            .or_else(|| flag_value(&tokens, "--override_ide_name"))
            .unwrap_or_default()
            .to_lowercase();
        let app_data = flag_value(&tokens, "--app_data_dir").unwrap_or_default().to_lowercase();
        let is_antigravity = ide_name == "antigravity"
            || ide_name == "antigravity-ide"
            || app_data.contains("antigravity");
        if !is_antigravity {
            continue;
        }

        let csrf_token = flag_value(&tokens, "--csrf_token").unwrap_or_default();
        let extension_port = flag_value(&tokens, "--extension_server_port")
            .and_then(|v| v.parse::<u16>().ok());

        let mut ports: Vec<u16> = netstat
            .lines()
            .filter(|line| line.contains("LISTENING") && line.trim().ends_with(&pid.to_string()))
            .filter_map(|line| {
                let local = line.split_whitespace().nth(1)?;
                let (addr, port) = local.rsplit_once(':')?;
                if addr == "127.0.0.1" || addr == "0.0.0.0" {
                    port.parse::<u16>().ok()
                } else {
                    None
                }
            })
            .collect();
        ports.sort_unstable();
        ports.dedup();

        if csrf_token.is_empty() || (ports.is_empty() && extension_port.is_none()) {
            continue;
        }
        found.push(LanguageServer { ports, extension_port, csrf_token });
    }
    found
}

// ---------------------------------------------------------------------------
// Language-server RPC
// ---------------------------------------------------------------------------

/// The LS uses a self-signed cert on loopback; this client is only ever
/// pointed at 127.0.0.1. A legit server never redirects (a redirect would
/// carry the csrf header cross-origin) and plaintext loopback must never
/// ride a proxy.
fn ls_client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| super::http())
}

async fn ls_call(scheme: &str, port: u16, csrf: &str, method: &str) -> Option<Value> {
    let url = format!("{scheme}://127.0.0.1:{port}/{LS_SERVICE}/{method}");
    let body = json!({
        "metadata": {
            "ideName": "antigravity",
            "extensionName": "antigravity",
            "ideVersion": "unknown",
            "locale": "en",
        }
    });
    let resp = ls_client()
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Connect-Protocol-Version", "1")
        .header("x-codeium-csrf-token", csrf)
        .json(&body)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<Value>().await.ok()
}

async fn try_language_server(server: &LanguageServer) -> Option<Snapshot> {
    let mut attempts: Vec<(String, u16)> = Vec::new();
    for scheme in ["https", "http"] {
        for port in &server.ports {
            attempts.push((scheme.to_string(), *port));
        }
    }
    if let Some(ext) = server.extension_port {
        attempts.push(("http".into(), ext));
    }

    for (scheme, port) in attempts {
        // Authoritative endpoint first: merged pools + weekly windows.
        if let Some(doc) =
            ls_call(&scheme, port, &server.csrf_token, "RetrieveUserQuotaSummary").await
        {
            let payload = doc.get("response").unwrap_or(&doc);
            let metrics = parse_quota_buckets(payload);
            if !metrics.is_empty() {
                let plan = ls_call(&scheme, port, &server.csrf_token, "GetUserStatus")
                    .await
                    .and_then(|d| extract_plan(&d));
                return Some(Snapshot::ok(ID, NAME, plan, metrics));
            }
        }
        // Legacy: per-model configs pooled into Session/Claude.
        if let Some(doc) = ls_call(&scheme, port, &server.csrf_token, "GetUserStatus").await {
            let metrics = parse_model_configs(&doc);
            if !metrics.is_empty() {
                let plan = extract_plan(&doc);
                return Some(Snapshot::ok(ID, NAME, plan, metrics));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Cloud Code fallback (works while the IDE is closed)
// ---------------------------------------------------------------------------

enum CloudResult {
    Ok(Snapshot),
    AuthExpired,
    Unavailable,
    NoCredentials,
}

struct StoredToken {
    access_token: String,
    refresh_token: Option<String>,
    expires_at_ms: Option<i64>,
}

/// Antigravity stores its Google OAuth token via go-keyring:
/// `go-keyring-base64:<base64 of {"token":{access_token,refresh_token,expiry}}>`.
fn load_stored_token() -> Option<StoredToken> {
    let json_text = super::credential_string("gemini:antigravity")?;
    let doc: Value = serde_json::from_str(json_text.trim()).ok()?;
    let token = doc.get("token").unwrap_or(&doc);
    let access = token.get("access_token").and_then(Value::as_str)?.to_string();
    let refresh = token.get("refresh_token").and_then(Value::as_str).map(str::to_string);
    let expires_at_ms = token
        .get("expiry")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis());
    Some(StoredToken { access_token: access, refresh_token: refresh, expires_at_ms })
}

fn cached_token_path() -> std::path::PathBuf {
    super::config_dir().join("antigravity-token.json")
}

fn load_cached_refresh() -> Option<(String, i64)> {
    let doc: Value =
        serde_json::from_str(&std::fs::read_to_string(cached_token_path()).ok()?).ok()?;
    Some((
        doc.get("accessToken")?.as_str()?.to_string(),
        doc.get("expiresAtMs")?.as_i64()?,
    ))
}

fn save_cached_refresh(access_token: &str, expires_at_ms: i64) {
    let _ = std::fs::create_dir_all(super::config_dir());
    let _ = std::fs::write(
        cached_token_path(),
        json!({ "accessToken": access_token, "expiresAtMs": expires_at_ms }).to_string(),
    );
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

enum Refresh {
    Refreshed(String, i64),
    AuthFailed,
    Unavailable,
}

async fn refresh_google_token(refresh_token: &str) -> Refresh {
    let form = [
        ("client_id", GOOGLE_CLIENT_ID),
        ("client_secret", GOOGLE_CLIENT_SECRET),
        ("refresh_token", refresh_token),
        ("grant_type", "refresh_token"),
    ];
    let resp = match super::http()
        .post("https://oauth2.googleapis.com/token")
        .form(&form)
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return Refresh::Unavailable,
    };
    let status = resp.status();
    if status.is_success() {
        if let Ok(doc) = resp.json::<Value>().await {
            if let Some(access) = doc.get("access_token").and_then(Value::as_str) {
                let expires_in = doc.get("expires_in").and_then(Value::as_i64).unwrap_or(3600);
                return Refresh::Refreshed(access.to_string(), now_ms() + expires_in * 1000);
            }
        }
        Refresh::Unavailable
    } else if status.as_u16() == 408 || status.as_u16() == 429 {
        Refresh::Unavailable
    } else if status.is_client_error() {
        Refresh::AuthFailed
    } else {
        Refresh::Unavailable
    }
}

async fn cloud_call(access_token: &str, path: &str) -> Result<Option<Value>, bool> {
    // Ok(Some(doc)) = 2xx; Ok(None) = non-auth failure; Err(true) = 401/403.
    for base in CLOUD_BASES {
        let resp = super::http()
            .post(format!("{base}{path}"))
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {access_token}"))
            .header("User-Agent", "antigravity")
            .json(&json!({}))
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => return Ok(r.json::<Value>().await.ok()),
            Ok(r) if matches!(r.status().as_u16(), 401 | 403) => return Err(true),
            _ => continue,
        }
    }
    Ok(None)
}

async fn cloud_snapshot(access_token: &str) -> Result<Option<Snapshot>, bool> {
    let doc = cloud_call(access_token, "/v1internal:retrieveUserQuotaSummary").await?;
    if let Some(doc) = doc {
        let metrics = parse_quota_buckets(&doc);
        if !metrics.is_empty() {
            let plan = match cloud_call(access_token, "/v1internal:loadCodeAssist").await {
                Ok(Some(d)) => d
                    .get("paidTier")
                    .or_else(|| d.get("currentTier"))
                    .and_then(|t| t.get("name"))
                    .and_then(Value::as_str)
                    .map(format_plan),
                _ => None,
            };
            return Ok(Some(Snapshot::ok(ID, NAME, plan, metrics)));
        }
    }
    Ok(None)
}

async fn try_cloud() -> CloudResult {
    let Some(stored) = load_stored_token() else {
        return CloudResult::NoCredentials;
    };

    // Freshest first: a still-valid cached refresh, then the stored token.
    let mut candidates: Vec<String> = Vec::new();
    if let Some((access, expires)) = load_cached_refresh() {
        if expires - now_ms() > 60_000 {
            candidates.push(access);
        }
    }
    let stored_valid = stored.expires_at_ms.map(|e| e - now_ms() > 60_000).unwrap_or(true);
    if stored_valid {
        candidates.push(stored.access_token.clone());
    }

    let mut auth_failed = false;
    for token in &candidates {
        match cloud_snapshot(token).await {
            Ok(Some(snap)) => return CloudResult::Ok(snap),
            Ok(None) => return CloudResult::Unavailable,
            Err(_) => auth_failed = true,
        }
    }

    // Tokens rejected or expired — refresh and retry once.
    if auth_failed || !stored_valid || candidates.is_empty() {
        let Some(refresh_token) = stored.refresh_token else {
            return if auth_failed { CloudResult::AuthExpired } else { CloudResult::NoCredentials };
        };
        match refresh_google_token(&refresh_token).await {
            Refresh::Refreshed(access, expires_at) => {
                save_cached_refresh(&access, expires_at);
                match cloud_snapshot(&access).await {
                    Ok(Some(snap)) => CloudResult::Ok(snap),
                    Ok(None) => CloudResult::Unavailable,
                    Err(_) => CloudResult::AuthExpired,
                }
            }
            Refresh::AuthFailed => CloudResult::AuthExpired,
            Refresh::Unavailable => CloudResult::Unavailable,
        }
    } else {
        CloudResult::Unavailable
    }
}

// ---------------------------------------------------------------------------
// Response parsing (shared by LS and Cloud Code shapes)
// ---------------------------------------------------------------------------

const FIVE_HOURS_MS: i64 = 5 * 60 * 60 * 1000;
const ONE_WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;

fn bucket_label(bucket_id: &str) -> Option<(&'static str, i64, usize)> {
    // label, period, sort order — exact bucketId match like the Mac app.
    match bucket_id {
        "gemini-5h" => Some(("Session", FIVE_HOURS_MS, 0)),
        "gemini-weekly" => Some(("Weekly", ONE_WEEK_MS, 1)),
        "3p-5h" => Some(("Claude", FIVE_HOURS_MS, 2)),
        "3p-weekly" => Some(("Claude Weekly", ONE_WEEK_MS, 3)),
        _ => None,
    }
}

fn parse_iso_ms(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis())
}

fn parse_quota_buckets(doc: &Value) -> Vec<Metric> {
    let mut rows: Vec<(usize, Metric)> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let groups = doc.get("groups").and_then(Value::as_array).cloned().unwrap_or_default();
    for group in groups {
        let buckets = group.get("buckets").and_then(Value::as_array).cloned().unwrap_or_default();
        for bucket in buckets {
            let Some(id) = bucket.get("bucketId").and_then(Value::as_str) else { continue };
            if seen.iter().any(|s| s == id) {
                continue;
            }
            let Some((label, period_ms, order)) = bucket_label(id) else { continue };
            let Some(remaining) = bucket.get("remainingFraction").and_then(Value::as_f64) else {
                continue;
            };
            if !remaining.is_finite() {
                continue;
            }
            seen.push(id.to_string());
            let used = ((1.0 - remaining) * 100.0).clamp(0.0, 100.0);
            let resets_at = parse_iso_ms(bucket.get("resetTime"));
            rows.push((
                order,
                Metric::progress(label, used, None).with_reset(resets_at, Some(period_ms)),
            ));
        }
    }
    rows.sort_by_key(|(order, _)| *order);
    rows.into_iter().map(|(_, m)| m).collect()
}

/// Legacy shape: per-model quotas pooled into "Session" (Gemini) and
/// "Claude" (everything else), keeping each pool's worst remaining fraction.
fn parse_model_configs(doc: &Value) -> Vec<Metric> {
    let configs = doc
        .pointer("/userStatus/cascadeModelConfigData/clientModelConfigs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut pools: [Option<(f64, Option<i64>)>; 2] = [None, None]; // [gemini, other]
    for cfg in configs {
        let label = cfg.get("label").and_then(Value::as_str).unwrap_or_default();
        let Some(remaining) = cfg.pointer("/quotaInfo/remainingFraction").and_then(Value::as_f64)
        else {
            continue;
        };
        let resets_at = parse_iso_ms(cfg.pointer("/quotaInfo/resetTime"));
        let idx = if label.to_lowercase().contains("gemini") { 0 } else { 1 };
        let worse = match pools[idx] {
            Some((existing, _)) => remaining < existing,
            None => true,
        };
        if worse {
            pools[idx] = Some((remaining, resets_at));
        }
    }
    let mut metrics = Vec::new();
    for (idx, label) in [(0, "Session"), (1, "Claude")] {
        if let Some((remaining, resets_at)) = pools[idx] {
            let used = ((1.0 - remaining) * 100.0).clamp(0.0, 100.0);
            metrics.push(
                Metric::progress(label, used, None).with_reset(resets_at, Some(FIVE_HOURS_MS)),
            );
        }
    }
    metrics
}

fn format_plan(raw: &str) -> String {
    let stripped = raw.trim().trim_start_matches("Google AI ").trim();
    for tier in ["Ultra", "Pro", "Free"] {
        if stripped.contains(tier) {
            return tier.to_string();
        }
    }
    stripped.to_string()
}

fn extract_plan(doc: &Value) -> Option<String> {
    let raw = doc
        .pointer("/userStatus/userTier/name")
        .or_else(|| doc.pointer("/userStatus/planStatus/planInfo/planName"))
        .and_then(Value::as_str)?;
    Some(format_plan(raw))
}

// ---------------------------------------------------------------------------
// Local spend events — token accounting in ~/.gemini/antigravity* stores
// ---------------------------------------------------------------------------
//
// Each Antigravity surface (agy CLI, IDE, Antigravity 2.0, ACP) keeps its
// own SQLite store under `~/.gemini/<dir>/conversations/*.db`. Per-turn
// usage lives in `gen_metadata.data` protobuf blobs; the transcript .pb
// files carry no usable token counts. Everything below is read-only:
// live files are opened SQLITE_OPEN_READ_ONLY and never written.

use super::minimax::{file_stamp, FileStamp};
use rusqlite::OptionalExtension;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// One decoded generation. `input` is the billed input — Antigravity
/// counts system-prompt tokens separately but bills them together.
#[derive(Clone, Debug)]
pub struct AgyEvent {
    pub ts_ms: i64,
    pub model_id: Option<String>,
    pub label: Option<String>,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
}

/// Generation blobs can run to hundreds of MB in real sessions; the SQL
/// CASE keeps anything past this from ever being materialised.
const MAX_AGY_BLOB_BYTES: i64 = 1_048_576;

fn wal_path(db: &Path) -> PathBuf {
    let mut p = db.as_os_str().to_os_string();
    p.push("-wal");
    PathBuf::from(p)
}

/// (mtime, size) pair for change detection; a missing file is the epoch.
fn latest_mtime(db: &Path) -> SystemTime {
    file_stamp(db).0.max(file_stamp(&wal_path(db)).0)
}

/// Creation time of the store file — a replaced or recreated db reuses
/// idx values, so size/mtime alone can't tell it apart from the original.
fn file_identity(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.created()).ok()
}

/// The `*.db` files under every `~/.gemini/antigravity*/conversations`,
/// deduplicated two ways: canonical path (symlinked store aliases) and
/// file stem (a copy-style store like `antigravity-backup` can hold the
/// same `<uuid>.db`; the freshest copy — latest db/WAL mtime — wins).
fn conversation_dbs(gemini: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(gemini) else {
        return Vec::new();
    };
    let mut stores: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path().join("conversations"))
        .filter(|p| {
            p.parent()
                .and_then(|d| d.file_name())
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("antigravity"))
        })
        .collect();
    stores.sort();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut by_stem: HashMap<String, (PathBuf, SystemTime)> = HashMap::new();
    for dir in stores {
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut dbs: Vec<PathBuf> = files
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("db"))
            .collect();
        dbs.sort();
        for path in dbs {
            let canon = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if !seen.insert(canon) {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let mtime = latest_mtime(&path);
            match by_stem.get(&stem) {
                Some((_, kept)) if *kept >= mtime => {}
                _ => {
                    by_stem.insert(stem, (path, mtime));
                }
            }
        }
    }
    let mut out: Vec<PathBuf> = by_stem.into_values().map(|(p, _)| p).collect();
    out.sort();
    out
}

// --- protobuf decode: hand-rolled varint/length-delimited walker ---------

enum ProtoVal<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

fn proto_varint(b: &[u8], offset: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut o = offset;
    for i in 0..10 {
        let x = *b.get(o)?;
        // Tenth byte may only carry the top bit of a u64.
        if i == 9 && (x & 0x7f) > 1 {
            return None;
        }
        value |= ((x & 0x7f) as u64) << (7 * i);
        o += 1;
        if x & 0x80 == 0 {
            return Some((value, o));
        }
    }
    None
}

/// First occurrence of field `want`; wire types 0/2 produce a value, 1/5
/// are skipped by width, anything else stops the walk (malformed).
fn proto_field(b: &[u8], want: u32) -> Option<ProtoVal<'_>> {
    let mut o = 0usize;
    while o < b.len() {
        let (tag, after) = proto_varint(b, o)?;
        let num = u32::try_from(tag >> 3).ok()?;
        if num == 0 {
            return None;
        }
        match tag & 7 {
            0 => {
                let (v, next) = proto_varint(b, after)?;
                if num == want {
                    return Some(ProtoVal::Varint(v));
                }
                o = next;
            }
            2 => {
                let (len, start) = proto_varint(b, after)?;
                let len = usize::try_from(len).ok()?;
                let end = start.checked_add(len)?;
                if end > b.len() {
                    return None;
                }
                if num == want {
                    return Some(ProtoVal::Bytes(&b[start..end]));
                }
                o = end;
            }
            1 | 5 => {
                let next = after.checked_add(if tag & 7 == 1 { 8 } else { 4 })?;
                if next > b.len() {
                    return None;
                }
                o = next;
            }
            _ => return None,
        }
    }
    None
}

fn bytes_field(b: &[u8], n: u32) -> Option<&[u8]> {
    match proto_field(b, n) {
        Some(ProtoVal::Bytes(v)) => Some(v),
        _ => None,
    }
}

fn varint_field(b: &[u8], n: u32) -> Option<u64> {
    match proto_field(b, n) {
        Some(ProtoVal::Varint(v)) => Some(v),
        _ => None,
    }
}

/// Seconds from a `google.protobuf.Timestamp` message (field 1).
fn timestamp_seconds(msg: &[u8]) -> Option<i64> {
    varint_field(msg, 1)
        .and_then(|s| i64::try_from(s).ok())
        .filter(|s| *s > 0)
}

fn trimmed_string(b: &[u8], n: u32) -> Option<String> {
    let s = String::from_utf8_lossy(bytes_field(b, n)?);
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// What one `gen_metadata` row decodes to.
enum Decoded {
    /// A generation with a usable timestamp.
    Dated(AgyEvent),
    /// A real generation with no timestamp yet — worth retrying once its
    /// `steps.metadata` lands.
    Undated,
    /// Bookkeeping or noise — never produces an event.
    Skip,
}

/// `gen_metadata.data` wraps its event in field 1: model id 19, display
/// label 21, usage 4 (1=system-prompt, 2=input, 3=output, 5=cache-read),
/// timing 9 (whose field 4 is a Timestamp). Rows with neither name that
/// carry only a system-prompt count are prompt-context bookkeeping, not
/// generations. `steps.metadata` field 1 is the timestamp fallback —
/// file mtimes move on every write, so undated rows are kept pending
/// for a later scan instead of being dated by a file time.
fn decode_event(data: &[u8], step: Option<&[u8]>) -> Decoded {
    let Some(w) = bytes_field(data, 1) else {
        return Decoded::Skip;
    };
    let model_id = trimmed_string(w, 19);
    let label = trimmed_string(w, 21);
    let Some(usage) = bytes_field(w, 4) else {
        return Decoded::Skip;
    };
    let sys = varint_field(usage, 1).unwrap_or(0);
    let input = varint_field(usage, 2).unwrap_or(0);
    let output = varint_field(usage, 3).unwrap_or(0);
    let cache_read = varint_field(usage, 5).unwrap_or(0);
    let Some(billed) = sys.checked_add(input) else {
        return Decoded::Skip;
    };
    let generated = input != 0 || output != 0 || cache_read != 0;
    if !(model_id.is_some() || label.is_some() || generated) {
        return Decoded::Skip;
    }
    if !(generated || billed != 0) {
        return Decoded::Skip;
    }
    let Some(ts) = bytes_field(w, 9)
        .and_then(|t| bytes_field(t, 4))
        .and_then(timestamp_seconds)
        .or_else(|| {
            step.and_then(|s| bytes_field(s, 1))
                .and_then(timestamp_seconds)
        })
    else {
        return Decoded::Undated;
    };
    let Some(ts_ms) = ts.checked_mul(1000) else {
        return Decoded::Skip;
    };
    Decoded::Dated(AgyEvent {
        ts_ms,
        model_id,
        label,
        input: billed,
        output,
        cache_read,
    })
}

// --- per-store cached read ------------------------------------------------

/// Undated generations held for retry. Past the cap the oldest idx is
/// dropped so a store full of never-dated rows cannot force every scan
/// to re-read the whole table.
const MAX_PENDING_IDXS: usize = 512;

/// One store's decoded events plus the read cursor. Rows are append-only
/// by `idx`, so a changed store resumes where it left off instead of
/// re-decoding every blob.
struct AgyDbCache {
    db: FileStamp,
    wal: FileStamp,
    identity: Option<SystemTime>,
    last_idx: i64,
    /// Generation idx values that decoded real but undated — retried on
    /// the next store change, when a steps row may have gained metadata.
    pending: BTreeSet<i64>,
    events: Vec<AgyEvent>,
    /// Whether `steps.metadata` exists; None until first probed, and
    /// re-probed while false so a store that gains it later is re-read.
    has_step_meta: Option<bool>,
}

impl Default for AgyDbCache {
    fn default() -> Self {
        Self {
            db: (SystemTime::UNIX_EPOCH, 0),
            wal: (SystemTime::UNIX_EPOCH, 0),
            identity: None,
            last_idx: -1,
            pending: BTreeSet::new(),
            events: Vec::new(),
            has_step_meta: None,
        }
    }
}

fn read_db(path: &Path, entry: &mut AgyDbCache) -> Result<(), String> {
    let conn = super::open_readonly_sqlite(path)?;
    // A store without gen_metadata simply has no usage — not an error.
    let has_gen: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='gen_metadata'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("probe gen_metadata: {e}"))?;
    if has_gen == 0 {
        return Ok(());
    }
    if entry.has_step_meta != Some(true) {
        let has = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('steps') WHERE name='metadata'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map_err(|e| format!("probe steps.metadata: {e}"))?
            .is_some();
        if has && entry.has_step_meta == Some(false) {
            // Rows dropped earlier for having no timestamp may now be
            // datable — restart the cursor so they are re-read.
            entry.last_idx = -1;
            entry.events.clear();
            entry.pending.clear();
        }
        entry.has_step_meta = Some(has);
    }
    let step_expr = if entry.has_step_meta == Some(true) {
        format!(
            "(SELECT CASE WHEN length(s.metadata) <= {MAX_AGY_BLOB_BYTES} THEN s.metadata END FROM steps s WHERE s.idx = g.idx)"
        )
    } else {
        "NULL".to_string()
    };
    // Only fixed literals are interpolated. The CASE arms keep oversized
    // blobs out of memory — they arrive as NULL and are skipped.
    let sql = format!(
        "SELECT g.idx,
                CASE WHEN length(g.data) <= {MAX_AGY_BLOB_BYTES} THEN g.data END,
                {step_expr}
         FROM gen_metadata g
         WHERE g.data IS NOT NULL AND g.idx > ?1
         ORDER BY g.idx
         LIMIT {}",
        super::MAX_LEDGER_ROWS
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("query gen_metadata: {e}"))?;
    // Pending rows sit at/below the cursor — reach back to the oldest
    // one so a step timestamp that landed since gets a second pass.
    let lower = entry
        .pending
        .first()
        .map(|min| min - 1)
        .unwrap_or(entry.last_idx);
    let rows = stmt
        .query_map([lower], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<Vec<u8>>>(1)?,
                row.get::<_, Option<Vec<u8>>>(2)?,
            ))
        })
        .map_err(|e| format!("read gen_metadata: {e}"))?;
    for row in rows.flatten() {
        let (idx, blob, step) = row;
        // Rows at/below the cursor are already counted — only pending
        // ones are decoded again.
        if idx <= entry.last_idx && !entry.pending.contains(&idx) {
            continue;
        }
        entry.last_idx = entry.last_idx.max(idx);
        let Some(blob) = blob else {
            entry.pending.remove(&idx);
            continue;
        };
        match decode_event(&blob, step.as_deref()) {
            Decoded::Dated(ev) => {
                entry.events.push(ev);
                entry.pending.remove(&idx);
            }
            Decoded::Undated => {
                entry.pending.insert(idx);
                while entry.pending.len() > MAX_PENDING_IDXS {
                    entry.pending.pop_first();
                }
            }
            Decoded::Skip => {
                entry.pending.remove(&idx);
            }
        }
    }
    Ok(())
}

/// Generation events across every `antigravity*` store. Per-DB events are
/// cached on the (db, WAL) stamps; a busy or locked store serves its last
/// good events and is retried on the next scan. Stores untouched since
/// `cutoff` are skipped without opening them.
pub fn collect_usage_events(cutoff: SystemTime) -> Vec<AgyEvent> {
    static CACHE: Mutex<Option<HashMap<PathBuf, AgyDbCache>>> = Mutex::new(None);
    let Some(gemini) = dirs::home_dir().map(|h| h.join(".gemini")) else {
        return Vec::new();
    };
    let paths = conversation_dbs(&gemini);
    let live: HashSet<&PathBuf> = paths.iter().collect();
    let Ok(mut guard) = CACHE.lock() else {
        return Vec::new();
    };
    let cache = guard.get_or_insert_with(HashMap::new);
    cache.retain(|k, _| live.contains(k));
    let cutoff_ms = cutoff
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let mut out = Vec::new();
    for path in paths {
        let db = file_stamp(&path);
        let wal = file_stamp(&wal_path(&path));
        if db.0.max(wal.0) < cutoff {
            continue;
        }
        let identity = file_identity(&path);
        let entry = cache.entry(path.clone()).or_default();
        if entry.db != db || entry.wal != wal {
            // A different file or a shrunk one restarts the cursor —
            // idx values only mean append-order within one store lineage.
            if (entry.identity.is_some() && entry.identity != identity) || entry.db.1 > db.1 {
                entry.last_idx = -1;
                entry.events.clear();
                entry.pending.clear();
            }
            entry.identity = identity;
            if read_db(&path, entry).is_ok() {
                entry.db = db;
                entry.wal = wal;
            }
            // Busy/locked: keep the old stamps so the next scan retries;
            // last good events are still served below.
        }
        out.extend(
            entry
                .events
                .iter()
                .filter(|e| e.ts_ms >= cutoff_ms)
                .cloned(),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- tiny protobuf builders for hand-rolled fixtures -------------------

    fn pvarint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let x = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(x);
                return;
            }
            out.push(x | 0x80);
        }
    }

    fn ptag(num: u32, wt: u32, out: &mut Vec<u8>) {
        pvarint(((num << 3) | wt) as u64, out);
    }

    fn pbytes(num: u32, data: &[u8], out: &mut Vec<u8>) {
        ptag(num, 2, out);
        pvarint(data.len() as u64, out);
        out.extend_from_slice(data);
    }

    fn pvar_field(num: u32, v: u64, out: &mut Vec<u8>) {
        ptag(num, 0, out);
        pvarint(v, out);
    }

    fn pstr(num: u32, s: &str, out: &mut Vec<u8>) {
        pbytes(num, s.as_bytes(), out);
    }

    /// usage{1: sys, 2: input, 3: output, 5: cache_read}
    fn usage_msg(sys: u64, input: u64, output: u64, cr: u64) -> Vec<u8> {
        let mut u = Vec::new();
        for (n, v) in [(1, sys), (2, input), (3, output), (5, cr)] {
            if v != 0 {
                pvar_field(n, v, &mut u);
            }
        }
        u
    }

    /// Timestamp{1: seconds}
    fn ts_msg(secs: u64) -> Vec<u8> {
        let mut t = Vec::new();
        pvar_field(1, secs, &mut t);
        t
    }

    /// gen_metadata.data: field 1 wraps {19: id, 21: label, 4: usage,
    /// 9: {4: Timestamp}}.
    fn data_blob(id: Option<&str>, label: Option<&str>, usage: &[u8], ts: Option<u64>) -> Vec<u8> {
        let mut w = Vec::new();
        if let Some(s) = id {
            pstr(19, s, &mut w);
        }
        if let Some(s) = label {
            pstr(21, s, &mut w);
        }
        pbytes(4, usage, &mut w);
        if let Some(secs) = ts {
            let mut timing = Vec::new();
            pbytes(4, &ts_msg(secs), &mut timing);
            pbytes(9, &timing, &mut w);
        }
        let mut data = Vec::new();
        pbytes(1, &w, &mut data);
        data
    }

    /// steps.metadata: field 1 is a Timestamp message.
    fn step_meta(secs: u64) -> Vec<u8> {
        let mut m = Vec::new();
        pbytes(1, &ts_msg(secs), &mut m);
        m
    }

    /// Test view of the tri-state decoder: just the dated event.
    fn decoded(data: &[u8], step: Option<&[u8]>) -> Option<AgyEvent> {
        match decode_event(data, step) {
            Decoded::Dated(e) => Some(e),
            _ => None,
        }
    }

    #[test]
    fn full_event_decodes_with_embedded_timestamp() {
        let blob = data_blob(
            Some("gemini-3.7-flash"),
            Some("Gemini 3.7 Flash (High)"),
            &usage_msg(100, 200, 50, 30),
            Some(1_700_000_000),
        );
        let ev = decoded(&blob, None).unwrap();
        assert_eq!(ev.model_id.as_deref(), Some("gemini-3.7-flash"));
        assert_eq!(ev.label.as_deref(), Some("Gemini 3.7 Flash (High)"));
        assert_eq!(ev.input, 300); // system-prompt + input billed together
        assert_eq!(ev.output, 50);
        assert_eq!(ev.cache_read, 30);
        assert_eq!(ev.ts_ms, 1_700_000_000_000);
    }

    #[test]
    fn steps_metadata_supplies_a_missing_timestamp() {
        let blob = data_blob(
            Some("gemini-3.8-flash"),
            None,
            &usage_msg(0, 10, 5, 0),
            None,
        );
        assert!(matches!(decode_event(&blob, None), Decoded::Undated));
        let ev = decoded(&blob, Some(&step_meta(1_700_000_100))).unwrap();
        assert_eq!(ev.ts_ms, 1_700_000_100_000);
        assert_eq!(ev.model_id.as_deref(), Some("gemini-3.8-flash"));
    }

    #[test]
    fn no_timestamp_anywhere_marks_the_row_pending() {
        let blob = data_blob(
            Some("gemini-3.8-flash"),
            None,
            &usage_msg(0, 10, 5, 0),
            None,
        );
        assert!(matches!(decode_event(&blob, None), Decoded::Undated));
        // A generation whose timing field has the wrong wire type is
        // still real — unusable timestamp, pending not skipped.
        let mut w = Vec::new();
        pstr(19, "gemini-3.8-flash", &mut w);
        pbytes(4, &usage_msg(0, 10, 5, 0), &mut w);
        pvar_field(9, 0, &mut w);
        let mut blob2 = Vec::new();
        pbytes(1, &w, &mut blob2);
        assert!(matches!(decode_event(&blob2, None), Decoded::Undated));
    }

    #[test]
    fn prompt_context_only_rows_drop() {
        // No id, no label, only a system-prompt count = bookkeeping.
        let blob = data_blob(None, None, &usage_msg(500, 0, 0, 0), Some(1_700_000_000));
        assert!(matches!(decode_event(&blob, None), Decoded::Skip));
        // No id/label and nothing at all → dropped too.
        let blob = data_blob(None, None, &usage_msg(0, 0, 0, 0), Some(1_700_000_000));
        assert!(matches!(decode_event(&blob, None), Decoded::Skip));
        // A labelled row with only prompt tokens is a real (if empty)
        // generation and survives with its billed input.
        let blob = data_blob(
            None,
            Some("Gemini 3.7 Flash"),
            &usage_msg(500, 0, 0, 0),
            Some(1_700_000_000),
        );
        let ev = decoded(&blob, None).unwrap();
        assert_eq!(ev.input, 500);
        assert_eq!(ev.output, 0);
    }

    #[test]
    fn malformed_blobs_fail_cleanly() {
        assert!(matches!(decode_event(&[], None), Decoded::Skip));
        assert!(matches!(decode_event(&[0xff; 32], None), Decoded::Skip));
        // Truncated length-delimited payload.
        let mut bad = Vec::new();
        ptag(1, 2, &mut bad);
        pvarint(500, &mut bad);
        bad.extend_from_slice(&[1, 2, 3]);
        assert!(matches!(decode_event(&bad, None), Decoded::Skip));
        // Wrapper present but no usage message.
        let mut w = Vec::new();
        pstr(19, "gemini-3.8-flash", &mut w);
        let mut blob = Vec::new();
        pbytes(1, &w, &mut blob);
        assert!(matches!(decode_event(&blob, None), Decoded::Skip));
    }

    #[test]
    fn conversation_dbs_dedupes_by_stem_across_stores() {
        let root = std::env::temp_dir().join(format!("pane-agy-dedup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for store in ["antigravity", "antigravity-backup", "antigravity-ide"] {
            std::fs::create_dir_all(root.join(store).join("conversations")).unwrap();
        }
        // Same conversation copied into the backup store — must not count twice.
        let live = root.join("antigravity/conversations/aaa.db");
        let copy = root.join("antigravity-backup/conversations/aaa.db");
        std::fs::write(&live, b"live").unwrap();
        std::fs::write(&copy, b"copy").unwrap();
        let unique = root.join("antigravity-ide/conversations/bbb.db");
        std::fs::write(&unique, b"uniq").unwrap();
        // A non-.db file and a non-antigravity dir are ignored.
        std::fs::write(root.join("antigravity/conversations/x.pb"), b"n").unwrap();
        std::fs::create_dir_all(root.join("other/conversations")).unwrap();
        std::fs::write(root.join("other/conversations/ccc.db"), b"o").unwrap();

        // Make the backup copy look newer — it wins the stem tiebreak.
        let newer = filetime(&live) + std::time::Duration::from_secs(60);
        touch_mtime(&copy, newer);

        let dbs = conversation_dbs(&root);
        let names: Vec<_> = dbs
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(dbs.len(), 2);
        assert!(names.contains(&"aaa.db".to_string()));
        assert!(names.contains(&"bbb.db".to_string()));
        let aaa = dbs
            .iter()
            .find(|p| p.file_stem().unwrap() == "aaa")
            .unwrap();
        assert!(aaa.starts_with(root.join("antigravity-backup")));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn filetime(p: &Path) -> SystemTime {
        std::fs::metadata(p).unwrap().modified().unwrap()
    }

    fn touch_mtime(p: &Path, t: SystemTime) {
        let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
        f.set_modified(t).unwrap();
    }

    #[test]
    fn read_db_reads_blobs_and_skips_oversized() {
        let dir = std::env::temp_dir().join(format!("pane-agy-db-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("conv.db");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB);
                 CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB);",
            )
            .unwrap();
            let good1 = data_blob(
                Some("gemini-3.8-flash"),
                None,
                &usage_msg(0, 10, 5, 0),
                Some(1_700_000_000),
            );
            // Row 2 has no embedded ts — steps.metadata supplies it.
            let good2 = data_blob(
                Some("gemini-3.6-flash"),
                None,
                &usage_msg(0, 20, 7, 0),
                None,
            );
            // Row 3 is oversized → CASE turns it NULL → skipped, cursor advances.
            let big = vec![0u8; (MAX_AGY_BLOB_BYTES + 1) as usize];
            conn.execute(
                "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1), (2, ?2), (3, ?3)",
                rusqlite::params![good1, good2, big],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO steps (idx, metadata) VALUES (2, ?1)",
                rusqlite::params![step_meta(1_700_000_500)],
            )
            .unwrap();
        }
        let mut entry = AgyDbCache::default();
        read_db(&db, &mut entry).unwrap();
        assert_eq!(entry.has_step_meta, Some(true));
        assert_eq!(entry.last_idx, 3);
        assert_eq!(entry.events.len(), 2);
        assert_eq!(
            entry.events[0].model_id.as_deref(),
            Some("gemini-3.8-flash")
        );
        assert_eq!(entry.events[0].input, 10);
        assert_eq!(entry.events[1].ts_ms, 1_700_000_500_000);
        assert_eq!(
            entry.events[1].model_id.as_deref(),
            Some("gemini-3.6-flash")
        );

        // A store without gen_metadata yields nothing, without error.
        let empty_db = dir.join("empty.db");
        {
            let conn = rusqlite::Connection::open(&empty_db).unwrap();
            conn.execute_batch("CREATE TABLE other (x INTEGER)")
                .unwrap();
        }
        let mut entry2 = AgyDbCache::default();
        read_db(&empty_db, &mut entry2).unwrap();
        assert!(entry2.events.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undated_generation_retries_until_its_step_lands() {
        let dir = std::env::temp_dir().join(format!("pane-agy-pending-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("conv.db");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB);
                 CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1), (2, ?2), (3, ?3)",
                rusqlite::params![
                    // Dated generation.
                    data_blob(
                        Some("gemini-3.8-flash"),
                        None,
                        &usage_msg(0, 10, 5, 0),
                        Some(1_700_000_000),
                    ),
                    // Real generation, no timing field — its steps row has
                    // NULL metadata, so it lands in pending.
                    data_blob(Some("gemini-3.6-flash"), None, &usage_msg(0, 20, 7, 0), None),
                    // Prompt-context bookkeeping: never pending, never counted.
                    data_blob(None, None, &usage_msg(500, 0, 0, 0), Some(1_700_000_000)),
                ],
            )
            .unwrap();
            conn.execute("INSERT INTO steps (idx, metadata) VALUES (2, NULL)", [])
                .unwrap();
        }
        let mut entry = AgyDbCache::default();
        read_db(&db, &mut entry).unwrap();
        assert_eq!(entry.events.len(), 1);
        assert_eq!(entry.last_idx, 3);
        assert!(entry.pending.contains(&2));
        assert!(!entry.pending.contains(&3));

        // The step's metadata lands and a new generation arrives; the
        // pending idx is retried and the scan still covers the new row.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute(
                "UPDATE steps SET metadata = ?1 WHERE idx = 2",
                rusqlite::params![step_meta(1_700_000_500)],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO gen_metadata (idx, data) VALUES (4, ?1)",
                rusqlite::params![data_blob(
                    Some("gemini-3.9-flash"),
                    None,
                    &usage_msg(0, 30, 9, 0),
                    Some(1_700_001_000),
                )],
            )
            .unwrap();
        }
        read_db(&db, &mut entry).unwrap();
        assert!(entry.pending.is_empty());
        assert_eq!(entry.last_idx, 4);
        assert_eq!(entry.events.len(), 3);
        let mut ids: Vec<_> = entry
            .events
            .iter()
            .map(|e| e.model_id.as_deref().unwrap())
            .collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            ["gemini-3.6-flash", "gemini-3.8-flash", "gemini-3.9-flash"]
        );
        // The retried row carries its step timestamp, exactly once.
        assert_eq!(
            entry
                .events
                .iter()
                .filter(|e| e.ts_ms == 1_700_000_500_000)
                .count(),
            1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Live probe against this machine's real credentials — run manually via
    /// `cargo test --lib antigravity -- --ignored --nocapture`. Prints field
    /// names and numbers only, never token values.
    #[test]
    #[ignore]
    fn live_probe() {
        let snap = tauri::async_runtime::block_on(super::snapshot());
        eprintln!(
            "antigravity: status={} plan={:?} error={:?} metrics={}",
            snap.status,
            snap.plan,
            snap.error,
            snap.metrics.len()
        );
        for m in &snap.metrics {
            eprintln!(
                "  {}: used={:?} resets_at={:?} period={:?}",
                m.label, m.used_percent, m.resets_at, m.period_ms
            );
        }
    }
}
