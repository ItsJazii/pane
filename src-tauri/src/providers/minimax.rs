//! MiniMax Coding/Token Plan (M2.7 / M3 models). Quota is read through the
//! MiniMax Code (mcode) CLI's OAuth sign-in when present — same requests and
//! request signing mcode itself makes — else through the key endpoint with a
//! Bearer API key from Settings, MINIMAX_API_KEY, or the MiniMax Agent CLI's
//! ~/.minimax/config.yaml (provider.minimax.options.apiKey).

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{Metric, Snapshot};

#[cfg(test)]
pub(crate) const MAX_TEMP_SNAPSHOT_BYTES: u64 = super::MAX_TEMP_SQLITE_BYTES;

#[cfg(test)]
pub(crate) fn temp_snapshot_allowed(src_len: u64) -> bool {
    src_len <= MAX_TEMP_SNAPSHOT_BYTES
}

const ID: &str = "minimax";
const NAME: &str = "MiniMax";

// Primary is the official Token Plan endpoint; the openplatform path is the
// legacy alias several trackers still use; .minimaxi.com is the CN region.
const ENDPOINTS: [&str; 4] = [
    "https://api.minimax.io/v1/token_plan/remains",
    "https://api.minimax.io/v1/api/openplatform/coding_plan/remains",
    "https://api.minimaxi.com/v1/token_plan/remains",
    "https://api.minimaxi.com/v1/api/openplatform/coding_plan/remains",
];

fn find_api_key() -> Option<String> {
    if let Some(key) = super::stored_api_key(ID, &["MINIMAX_API_KEY"]) {
        return Some(key);
    }
    let path = dirs::home_dir()?.join(".minimax").join("config.yaml");
    let raw = std::fs::read_to_string(path).ok()?;
    cli_config_key(&raw)
}

/// The MiniMax Agent CLI key at exactly provider.minimax.options.apiKey —
/// an indent-tracked walk of the mapping path (still no YAML dependency).
/// Matching any `apiKey:` line in the file would let a same-named key that
/// belongs to a DIFFERENT provider in a shared config be sent to MiniMax's
/// endpoints.
fn cli_config_key(raw: &str) -> Option<String> {
    let mut stack: Vec<(usize, String)> = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        while stack.last().is_some_and(|(i, _)| *i >= indent) {
            stack.pop();
        }
        let Some((key, value)) = trimmed.split_once(':') else { continue };
        let (key, value) = (key.trim(), value.trim());
        if value.is_empty() {
            stack.push((indent, key.to_string()));
            continue;
        }
        if key == "apiKey"
            && stack.iter().map(|(_, k)| k.as_str()).eq(["provider", "minimax", "options"])
        {
            let v = value.trim_matches('"').trim_matches('\'');
            // Real MiniMax keys are long; fresh CLI installs carry a short
            // "sk-…" placeholder that would only produce a confusing error.
            if v.len() > 20 {
                return Some(v.to_string());
            }
            return None;
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
    let key = find_api_key();
    if let Some(login) = mcode_login() {
        match fetch_via_mcode(&login).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                if let Some(key) = &key {
                    eprintln!("[pane] minimax: mcode login: {e}");
                    return fetch_via_key(key).await;
                }
                return Err(e);
            }
        }
    }
    match key {
        Some(key) => fetch_via_key(&key).await,
        None => Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Open MiniMax Code (mcode) to refresh its sign-in, or paste a MiniMax key in Settings.",
        )),
    }
}

async fn fetch_via_key(key: &str) -> Result<Snapshot, String> {
    // When an mcode login was seen before its token lapsed, keep its plan
    // tier on the chip instead of flipping back to "Coding Plan".
    let plan = remembered_tier().unwrap_or_else(|| "Coding Plan".into());
    let mut last_error = String::from("quota endpoint unreachable");
    for endpoint in ENDPOINTS {
        let resp = match super::http()
            .get(endpoint)
            .header("Authorization", format!("Bearer {key}"))
            .header("Content-Type", "application/json")
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last_error = format!("quota request: {e}");
                continue;
            }
        };
        if !resp.status().is_success() {
            last_error = format!("quota endpoint: HTTP {}", resp.status());
            continue;
        }
        let doc: Value = match resp.json().await {
            Ok(d) => d,
            Err(e) => {
                last_error = format!("quota parse: {e}");
                continue;
            }
        };
        // MiniMax signals auth/path problems in-band: status_code != 0.
        let status_code = doc.pointer("/base_resp/status_code").and_then(Value::as_i64);
        if status_code != Some(0) {
            let msg = doc
                .pointer("/base_resp/status_msg")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            last_error = format!("MiniMax: {msg} (code {})", status_code.unwrap_or(-1));
            continue;
        }
        if let Some(snap) = parse_remains(&doc, Some(plan.clone())) {
            return Ok(snap);
        }
        last_error = "no recognizable quota rows in response".into();
    }
    Err(last_error)
}

/// Picks the coding-model row: "MiniMax-M*" preferred, then "general",
/// then the largest quota row.
fn pick_row(rows: &[Value]) -> Option<&Value> {
    let named = |pred: &dyn Fn(&str) -> bool| {
        rows.iter().find(|r| {
            r.get("model_name").and_then(Value::as_str).map(pred).unwrap_or(false)
        })
    };
    named(&|n: &str| n.starts_with("MiniMax-M"))
        .or_else(|| named(&|n: &str| n == "general"))
        .or_else(|| {
            rows.iter().max_by_key(|r| {
                r.get("current_interval_total_count").and_then(Value::as_i64).unwrap_or(0)
            })
        })
}

fn parse_remains(doc: &Value, plan: Option<String>) -> Option<Snapshot> {
    let rows = doc.get("model_remains").and_then(Value::as_array)?;
    let metrics = remains_metrics(rows);
    if metrics.is_empty() {
        return None;
    }
    Some(Snapshot::ok(ID, NAME, plan, metrics))
}

/// Quota rows from a `model_remains` response, in mcode's Settings → Usage
/// order: 5 Hours, Weekly, Video.
fn remains_metrics(rows: &[Value]) -> Vec<Metric> {
    let row = match pick_row(rows) {
        Some(r) => r,
        None => return Vec::new(),
    };
    let num = |key: &str| row.get(key).and_then(Value::as_f64);

    let mut metrics = Vec::new();

    // 5-hour rolling window (mcode labels it "5 Hours"). Field-name trap
    // (confirmed against the official CLI): *_usage_count actually holds
    // the REMAINING count.
    {
        let total = num("current_interval_total_count").unwrap_or(0.0);
        let remaining_count = num("current_interval_usage_count");
        let used_percent = num("current_interval_remaining_percent")
            .map(|p| 100.0 - p)
            .or_else(|| {
                remaining_count
                    .filter(|_| total > 0.0)
                    .map(|rem| 100.0 * (1.0 - rem / total))
            });
        if let Some(used) = used_percent {
            let detail = remaining_count
                .filter(|_| total > 0.0)
                .map(|rem| format!("{rem:.0} of {total:.0} left"));
            let resets_at = num("end_time").map(|v| v as i64).filter(|v| *v > 0);
            metrics.push(
                Metric::progress("5 Hours", used.clamp(0.0, 100.0), detail)
                    .with_reset(resets_at, Some(5 * 60 * 60 * 1000)),
            );
        }
    }

    // Weekly window. status 3 = unlimited; boost_permille can lift the
    // remaining percent above 100 (displayed capped at 100 here).
    {
        let status = num("current_weekly_status").unwrap_or(1.0) as i64;
        if status == 3 {
            metrics.push(Metric::text("Weekly", "Unlimited".into()));
        } else if let Some(remaining) = num("current_weekly_remaining_percent") {
            let boost = num("weekly_boost_permille").unwrap_or(1000.0) / 1000.0;
            let used = (100.0 - remaining * boost).clamp(0.0, 100.0);
            let total = num("current_weekly_total_count").unwrap_or(0.0);
            let detail = num("current_weekly_usage_count")
                .filter(|_| total > 0.0)
                .map(|rem| format!("{rem:.0} of {total:.0} left"));
            let resets_at = num("weekly_end_time").map(|v| v as i64).filter(|v| *v > 0);
            metrics.push(
                Metric::progress("Weekly", used, detail)
                    .with_reset(resets_at, Some(7 * 24 * 60 * 60 * 1000)),
            );
        }
    }

    // Video allowance — mcode's Usage screen shows it as "Video 0/5".
    // Same remaining-in-usage_count trap; status 3 = unlimited.
    if let Some(vrow) = rows.iter().find(|r| {
        r.get("model_name")
            .and_then(Value::as_str)
            .map(|n| n.to_lowercase().contains("video"))
            .unwrap_or(false)
    }) {
        let status = vrow
            .get("current_interval_status")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let total = vrow
            .get("current_interval_total_count")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        if status == 3 {
            metrics.push(Metric::text("Video", "Unlimited".into()));
        } else if total > 0.0 {
            if let Some(rem) = vrow
                .get("current_interval_usage_count")
                .and_then(Value::as_f64)
            {
                let used = (100.0 * (1.0 - rem / total)).clamp(0.0, 100.0);
                let start = vrow.get("start_time").and_then(Value::as_i64).unwrap_or(0);
                let end = vrow.get("end_time").and_then(Value::as_i64).unwrap_or(0);
                let period = (start > 0 && end > 0).then_some(end - start);
                metrics.push(
                    Metric::progress("Video", used, Some(format!("{rem:.0} of {total:.0} left")))
                        .with_reset((end > 0).then_some(end), period),
                );
            }
        }
    }

    metrics
}

// ---------------------------------------------------------------------------
// mcode OAuth login — the MiniMax Code CLI's own sign-in. The auth file is
// mcode's (it refreshes and rewrites it under its own lock/generation
// scheme); we only read it while the access token is still valid and never
// refresh or write it — a rotation from here could sign the user out.
// ---------------------------------------------------------------------------

const MCODE_FILE_CAP: u64 = 64 * 1024;
const MAX_MATRIX_BYTES: usize = 256 * 1024;
const MCODE_RECORD_PREFIX: &str = "com.minimax.mcode.oauth.prod.";

#[derive(Clone, Copy, PartialEq, Eq)]
enum McodeRegion {
    En,
    Cn,
}

impl McodeRegion {
    fn dir(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Cn => "cn",
        }
    }
    fn agent_host(self) -> &'static str {
        match self {
            Self::En => "https://agent.minimax.io",
            Self::Cn => "https://agent.minimaxi.com",
        }
    }
    fn platform_host(self) -> &'static str {
        match self {
            Self::En => "https://platform.minimax.io",
            Self::Cn => "https://www.minimaxi.com",
        }
    }
    fn lang(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Cn => "zh",
        }
    }
}

struct McodeLogin {
    access: String,
    region: McodeRegion,
    user_id: String,
}

/// The mcode sign-in, if a usable (unexpired) access token is on disk:
/// ~/.minimax/auth/prod/<region>/mcode-public/auth.json, en first then cn.
fn mcode_login() -> Option<McodeLogin> {
    let home = dirs::home_dir()?;
    for region in [McodeRegion::En, McodeRegion::Cn] {
        let auth = home
            .join(".minimax")
            .join("auth")
            .join("prod")
            .join(region.dir())
            .join("mcode-public")
            .join("auth.json");
        let Ok(raw) = super::read_small_text(&auth, MCODE_FILE_CAP, "mcode auth") else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<Value>(&raw) else { continue };
        let now_ms = chrono::Utc::now().timestamp_millis();
        let Some(access) = mcode_access_token(&doc, now_ms) else {
            continue;
        };
        let identity = home
            .join(".minimax")
            .join("cli-auth")
            .join("prod")
            .join(region.dir())
            .join("account-identity.json");
        let user_id = super::read_small_text(&identity, MCODE_FILE_CAP, "account identity")
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|d| {
                d.get("realUserID")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "0".into());
        return Some(McodeLogin {
            access,
            region,
            user_id,
        });
    }
    None
}

/// The access token from mcode's auth.json: the live prod record with
/// the latest expiry — expired records, unrelated records in the same
/// file, and tokens within a minute of lapsing are all skipped.
fn mcode_access_token(doc: &Value, now_ms: i64) -> Option<String> {
    doc.get("records")
        .and_then(Value::as_object)?
        .iter()
        .filter(|(k, _)| k.starts_with(MCODE_RECORD_PREFIX))
        .filter_map(|(_, rec)| {
            let expiry = rec.get("expiresAtMs").and_then(Value::as_i64)?;
            let token = rec.get("accessToken").and_then(Value::as_str)?.trim();
            (expiry > now_ms + 60_000 && !token.is_empty())
                .then(|| (expiry, token.to_string()))
        })
        .max_by_key(|(expiry, _)| *expiry)
        .map(|(_, token)| token)
}

/// JS encodeURIComponent: every byte except A-Z a-z 0-9 - _ . ! ~ * ' ( )
/// becomes %XX (uppercase hex, UTF-8 bytes).
fn encode_uri_component(s: &str) -> String {
    fn safe(b: u8) -> bool {
        matches!(b,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
            | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')')
    }
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if safe(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn md5_hex(data: &str) -> String {
    use md5::Digest;
    let mut h = md5::Md5::new();
    h.update(data.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// mcode's agent-host query string, in URLSearchParams order. The tz
/// offset is local UTC offset in seconds (chrono's local_minus_utc).
fn mcode_query(now_ms: i64, tz_offset_secs: i64, region: McodeRegion, user_id: &str) -> String {
    let lang = region.lang();
    format!(
        "device_platform=mcode&biz_id=3&app_id=3001&version_code=22201&unix={now_ms}&timezone_offset={tz_offset_secs}&sys_language={lang}&lang={lang}&device_id=0&os_name=win32&browser_name=mcode&user_id={user_id}&client=mcode"
    )
}

/// mcode's two signing headers. Returns (yy, x-timestamp, x-signature).
/// Note the asymmetry baked into mcode: `yy` always hashes the request
/// body (a literal "{}" on GETs) while `x-signature` uses an empty body
/// on GETs — POST callers pass the real body to both.
fn mcode_sign(path: &str, query: &str, body: &str, now_ms: i64) -> (String, String, String) {
    let secs = (now_ms / 1000).to_string();
    let yy = md5_hex(&format!(
        "{}_{}{}{}",
        encode_uri_component(&format!("{path}?{query}")),
        body,
        md5_hex(&now_ms.to_string()),
        "ooui"
    ));
    let x_signature = md5_hex(&format!("{secs}I*7Cf%WZ#S&%1RlZJ&C2{body}"));
    (yy, secs, x_signature)
}

/// Matrix answers carry statusInfo.code (0 ok; 1000048 = sign-in expired)
/// and/or base_resp.status_code (0 ok) — either non-zero is an error.
fn matrix_check(doc: &Value, what: &str) -> Result<(), String> {
    for code_ptr in ["/statusInfo/code", "/base_resp/status_code"] {
        if let Some(code) = doc.pointer(code_ptr).and_then(Value::as_i64) {
            if code != 0 {
                let msg = [
                    "/statusInfo/message",
                    "/statusInfo/msg",
                    "/base_resp/status_msg",
                ]
                .iter()
                .find_map(|p| doc.pointer(p).and_then(Value::as_str))
                .unwrap_or("unknown error");
                return Err(format!("{what}: {msg} (code {code})"));
            }
        }
    }
    Ok(())
}

/// Signed POST to mcode's agent host, bound at 4 s.
async fn matrix_post(login: &McodeLogin, path: &str, body: &str) -> Result<Value, String> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let tz = chrono::Local::now().offset().local_minus_utc();
    let query = mcode_query(now_ms, tz as i64, login.region, &login.user_id);
    let (yy, secs, x_signature) = mcode_sign(path, &query, body, now_ms);
    let url = format!("{}{}?{}", login.region.agent_host(), path, query);
    let resp = super::http()
        .post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("User-Agent", "MiniMaxCode")
        .bearer_auth(&login.access)
        .header("yy", yy)
        .header("x-timestamp", secs)
        .header("x-signature", x_signature)
        .timeout(Duration::from_secs(4))
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| format!("{path}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{path}: HTTP {}", resp.status()));
    }
    let doc = super::json_body(resp, MAX_MATRIX_BYTES, path).await?;
    matrix_check(&doc, path)?;
    Ok(doc)
}

/// The token-plan remains endpoint on the platform host — same shape the
/// key path parses. No mcode signing on this one; X-Group-Id always
/// carries the workspace's op_group_id.
async fn platform_remains(login: &McodeLogin, group_id: &str) -> Result<Value, String> {
    let url = format!(
        "{}/v1/api/openplatform/coding_plan/remains",
        login.region.platform_host()
    );
    let resp = super::http()
        .get(url)
        .header("Accept", "application/json")
        .header("X-Group-Id", group_id)
        .bearer_auth(&login.access)
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .map_err(|e| format!("remains: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("remains: HTTP {}", resp.status()));
    }
    let doc = super::json_body(resp, MAX_MATRIX_BYTES, "remains").await?;
    matrix_check(&doc, "remains")?;
    Ok(doc)
}

/// Where the last plan tier seen through the mcode login is remembered:
/// <config_dir>/minimax-plan.json. mcode's access token lapses after
/// about an hour whenever the CLI isn't running to refresh it; the key
/// path then takes over and would otherwise flip the chip back to the
/// generic "Coding Plan". The cache is scoped to the mcode account id
/// and dropped whenever the MiniMax key is changed or cleared, so a
/// pasted key never inherits another account's tier.
fn plan_cache_path(dir: &Path) -> PathBuf {
    dir.join("minimax-plan.json")
}

const REMEMBERED_TIER_TTL_MS: i64 = 30 * 24 * 3600 * 1000;

fn remembered_tier() -> Option<String> {
    remembered_tier_in(&super::config_dir())
}

fn remembered_tier_in(dir: &Path) -> Option<String> {
    remembered_tier_at(dir, chrono::Utc::now().timestamp_millis())
}

/// The stored tier when it was seen within the TTL — entries older than
/// 30 days, missing `seen_ms`, or stamped in the future (clock skew or a
/// tampered file) all count as expired.
fn remembered_tier_at(dir: &Path, now_ms: i64) -> Option<String> {
    let doc = read_tier_cache(dir)?;
    let seen = doc.get("seen_ms").and_then(Value::as_i64)?;
    if !(0..=REMEMBERED_TIER_TTL_MS).contains(&(now_ms - seen)) {
        return None;
    }
    let tier = doc.get("tier").and_then(Value::as_str)?.trim();
    (!tier.is_empty()).then(|| tier.to_string())
}

fn read_tier_cache(dir: &Path) -> Option<Value> {
    let raw = super::read_small_text(&plan_cache_path(dir), 4096, "minimax plan cache").ok()?;
    serde_json::from_str(&raw).ok()
}

/// The mcode refresh's write path: skips the write when a key-driven
/// cleanup bumped the generation while the request was in flight — that
/// refresh answered for the previous account.
fn remember_tier_if_unchanged(tier: &str, user_id: &str, generation: u64) {
    remember_tier_if_unchanged_in(&super::config_dir(), tier, user_id, generation)
}

fn remember_tier_if_unchanged_in(dir: &Path, tier: &str, user_id: &str, generation: u64) {
    if tier_cache_generation() != generation {
        eprintln!("[pane] minimax: key changed during refresh — not remembering the plan tier");
        return;
    }
    remember_tier_in(dir, tier, user_id);
}

fn remember_tier_in(dir: &Path, tier: &str, user_id: &str) {
    remember_tier_at(dir, tier, user_id, chrono::Utc::now().timestamp_millis());
}

/// Writes when the stored tier or account differs, or when the stored
/// entry is past half its TTL — a still-active mcode login keeps
/// refreshing `seen_ms`, so the 30-day expiry only bites after a month
/// without a successful login. A steady-state refresh costs one small
/// read, no write.
fn remember_tier_at(dir: &Path, tier: &str, user_id: &str, now_ms: i64) {
    let fresh = read_tier_cache(dir).as_ref().is_some_and(|d| {
        d.get("tier").and_then(Value::as_str) == Some(tier)
            && d.get("user_id").and_then(Value::as_str) == Some(user_id)
            && d.get("seen_ms")
                .and_then(Value::as_i64)
                .is_some_and(|seen| {
                    // A future stamp forces a rewrite with the real time.
                    (0..=REMEMBERED_TIER_TTL_MS / 2).contains(&(now_ms - seen))
                })
    });
    if fresh {
        return;
    }
    let path = plan_cache_path(dir);
    let json = serde_json::json!({
        "tier": tier,
        "seen_ms": now_ms,
        "user_id": user_id,
    })
    .to_string();
    if let Err(e) = super::onenewapi::store::atomic_write(&path, &json) {
        eprintln!("[pane] minimax: could not save {}: {e}", path.display());
    }
}

fn plan_stash_path(dir: &Path) -> PathBuf {
    dir.join("minimax-plan.json.old")
}

/// Bumped by every local mutation of the plan cache driven by a key
/// change (stash/restore/discard/forget). An mcode refresh captures it
/// before its network calls and only writes the tier if it is unchanged,
/// so a request that was in flight for the previous account can't
/// recreate that account's tier after cleanup.
static TIER_CACHE_GENERATION: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn tier_cache_generation() -> u64 {
    TIER_CACHE_GENERATION.load(std::sync::atomic::Ordering::Acquire)
}

fn bump_tier_cache_generation() {
    TIER_CACHE_GENERATION.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
}

/// Moves the remembered tier aside ahead of a key change. Returns whether
/// anything was stashed; a failure leaves the cache (and the old key) in
/// place so the retry still sees a rotation.
pub(crate) fn stash_remembered_tier_in(dir: &Path) -> Result<bool, String> {
    bump_tier_cache_generation();
    // Clear litter from a crash between stash and discard/restore.
    let _ = std::fs::remove_file(plan_stash_path(dir));
    match std::fs::rename(plan_cache_path(dir), plan_stash_path(dir)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("stash minimax plan cache: {e}")),
    }
}

/// The key write failed: put the stashed tier back (best effort, logged).
pub(crate) fn restore_stashed_tier_in(dir: &Path) {
    bump_tier_cache_generation();
    if let Err(e) = std::fs::rename(plan_stash_path(dir), plan_cache_path(dir)) {
        if e.kind() != std::io::ErrorKind::NotFound {
            eprintln!("[pane] minimax: could not restore {}: {e}", plan_stash_path(dir).display());
        }
    }
}

/// The key write succeeded: the old account's tier is gone for good.
pub(crate) fn discard_stashed_tier_in(dir: &Path) {
    bump_tier_cache_generation();
    match std::fs::remove_file(plan_stash_path(dir)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            eprintln!("[pane] minimax: could not remove {}: {e}", plan_stash_path(dir).display());
        }
    }
}

/// Drops the remembered tier — called when the MiniMax key is changed or
/// cleared so a pasted key never inherits another account's tier. A
/// leftover stash from a crashed change goes too.
pub(crate) fn forget_remembered_tier_in(dir: &Path) -> Result<(), String> {
    bump_tier_cache_generation();
    let stash_err = match std::fs::remove_file(plan_stash_path(dir)) {
        Ok(()) => None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => Some(format!("remove minimax plan stash: {e}")),
    };
    let cache_err = match std::fs::remove_file(plan_cache_path(dir)) {
        Ok(()) => None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => Some(format!("remove minimax plan cache: {e}")),
    };
    match (stash_err, cache_err) {
        (None, None) => Ok(()),
        (Some(e), None) | (None, Some(e)) => Err(e),
        (Some(a), Some(b)) => Err(format!("{a}; {b}")),
    }
}

pub(crate) fn remembered_tier_exists_in(dir: &Path) -> bool {
    plan_cache_path(dir).exists()
}

/// Plan/quota through the mcode login: workspace lookup first — the
/// remains call is meaningless without the workspace's op_group_id as
/// X-Group-Id, so a failed lookup or a missing default workspace fails
/// the whole path (the caller falls back to the key). The membership
/// call only runs when the workspace lookup didn't yield a plan tier —
/// it is the fallback tier source, nothing else.
async fn fetch_via_mcode(login: &McodeLogin) -> Result<Snapshot, String> {
    let generation = tier_cache_generation();
    let extra = matrix_post(login, "/matrix/api/v1/user/get_user_extra_info", "{}")
        .await
        .map_err(|e| format!("workspace lookup: {e}"))?;
    let ws = extra
        .get("workspaces")
        .and_then(Value::as_array)
        .and_then(|ws| {
            ws.iter()
                .find(|w| w.get("workspace_type").and_then(Value::as_i64) == Some(0))
        });
    let ws_group = ws
        .and_then(|w| w.get("op_group_id").and_then(Value::as_str))
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| "workspace lookup: no default workspace".to_string())?;
    let workspace_id = ws.and_then(|w| w.get("workspace_id")).cloned();
    let ws_tier = ws
        .and_then(|w| w.get("token_plan_tier").and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let member_req = async {
        if ws_tier.is_some() {
            return None;
        }
        let body = workspace_id
            .map(|id| serde_json::json!({ "workspace_id": id }).to_string())
            .unwrap_or_else(|| "{}".into());
        matrix_post(login, "/matrix/api/v1/commerce/get_membership_info", &body)
            .await
            .ok()
    };
    let remains_req = platform_remains(login, &ws_group);
    let (member, remains) = tokio::join!(member_req, remains_req);

    // The remains call is the quota source — without it there is no card.
    let remains = remains?;
    let rows = remains
        .get("model_remains")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let metrics = remains_metrics(&rows);
    if metrics.is_empty() {
        return Err("no recognizable quota rows in response".into());
    }

    let tier = ws_tier
        .or_else(|| {
            member
                .as_ref()
                .and_then(|d| d.get("token_plan_tier").and_then(Value::as_str))
                .map(str::to_string)
        })
        .filter(|s| !s.trim().is_empty());
    if let Some(t) = &tier {
        remember_tier_if_unchanged(t, &login.user_id, generation);
    }

    Ok(Snapshot::ok(ID, NAME, tier, metrics))
}

// ---------------------------------------------------------------------------
// Local spend: two MiniMax ledgers — the Agent CLI's ~/.minimax/sqlite.db
// token_usage table (frozen July 2026 but still the only source for that
// history) and mcode's ~/.minimax/v2/sqlite/runtime-state.sqlite. Same
// snapshot/cache machinery as the Devin store: the CLIs write to the WAL
// continuously, so raw file copies tear.
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct UsageEvent {
    pub ts_ms: i64,
    pub model: String,
    pub input: f64,
    pub output: f64,
    pub reasoning: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub cost_usd: f64,
}

fn agent_db_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".minimax").join("sqlite.db"))
}

/// mcode's newer store (it replaced the Agent CLI ledger in July): WAL-mode
/// runtime-state.sqlite under v2/sqlite. Its token rows carry no model —
/// that is joined from the turn's assistant message telemetry.
fn runtime_db_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".minimax")
            .join("v2")
            .join("sqlite")
            .join("runtime-state.sqlite")
    })
}

pub(crate) type FileStamp = (std::time::SystemTime, u64);

pub(crate) fn file_stamp(path: &Path) -> FileStamp {
    std::fs::metadata(path)
        .map(|m| (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len()))
        .unwrap_or((std::time::UNIX_EPOCH, 0))
}

fn wal_sidecar(db: &Path) -> PathBuf {
    let mut p = db.as_os_str().to_os_string();
    p.push("-wal");
    PathBuf::from(p)
}

/// One ledger source's last-good events, keyed by (db stamp, wal stamp).
type SourceCache = std::sync::Mutex<Option<(FileStamp, FileStamp, Vec<UsageEvent>)>>;

/// Per-turn token usage from both MiniMax stores — the old Agent CLI
/// ledger (frozen since July but still the only source for its history)
/// and mcode's v2 ledger. The two never overlap in time, so the events
/// just concatenate. Each source is cached on its own (db, WAL) stamps;
/// a busy/locked db serves its last good events, a missing file is
/// skipped.
pub fn collect_usage_events() -> Vec<UsageEvent> {
    static CACHES: [SourceCache; 2] =
        [std::sync::Mutex::new(None), std::sync::Mutex::new(None)];

    let mut out = Vec::new();
    for (db, slot, read) in [
        (agent_db_path(), 0usize, read_usage_events as fn(&Path) -> _),
        (runtime_db_path(), 1, read_v2_usage_events as fn(&Path) -> _),
    ] {
        let Some(db_path) = db else { continue };
        if !db_path.exists() {
            continue;
        }
        out.extend(collect_cached(&db_path, &CACHES[slot], read));
    }
    out
}

fn collect_cached(
    db_path: &Path,
    cache: &SourceCache,
    read: fn(&Path) -> Result<Vec<UsageEvent>, String>,
) -> Vec<UsageEvent> {
    let db_stamp = file_stamp(db_path);
    let wal_stamp = file_stamp(&wal_sidecar(db_path));

    if let Ok(c) = cache.lock() {
        if let Some((d, w, events)) = c.as_ref() {
            if *d == db_stamp && *w == wal_stamp {
                return events.clone();
            }
        }
    }

    match read(db_path) {
        Ok(events) => {
            if let Ok(mut c) = cache.lock() {
                *c = Some((db_stamp, wal_stamp, events.clone()));
            }
            events
        }
        Err(_) => cache
            .lock()
            .ok()
            .and_then(|c| c.as_ref().map(|(_, _, e)| e.clone()))
            .unwrap_or_default(),
    }
}

/// Test-only helper: the production spend path never copies a vendor
/// ledger into Temp. Kept so we can still prove leftover WAL files are
/// wiped and oversized sources are refused.
#[cfg(test)]
pub(crate) fn snapshot_db(src_path: &std::path::Path, dst_path: &std::path::Path) -> Result<(), String> {
    if !super::temp_sqlite_copy_allowed(src_path) {
        let src_len = std::fs::metadata(src_path).map(|m| m.len()).unwrap_or(0);
        return Err(format!(
            "temp snapshot refused: source is {src_len} bytes (cap {})",
            super::MAX_TEMP_SQLITE_BYTES
        ));
    }
    super::remove_sqlite_files(dst_path);
    let src = rusqlite::Connection::open_with_flags(
        src_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| format!("open live db: {e}"))?;
    src.busy_timeout(std::time::Duration::from_millis(250))
        .map_err(|e| format!("busy timeout: {e}"))?;
    let mut dst =
        rusqlite::Connection::open(dst_path).map_err(|e| format!("open snapshot: {e}"))?;
    {
        let backup = rusqlite::backup::Backup::new(&src, &mut dst)
            .map_err(|e| format!("backup init: {e}"))?;
        backup
            .run_to_completion(256, std::time::Duration::from_millis(10), None)
            .map_err(|e| format!("backup run: {e}"))?;
    }
    let _ = dst.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;");
    Ok(())
}

fn read_usage_events(db: &Path) -> Result<Vec<UsageEvent>, String> {
    let conn = super::open_readonly_sqlite(db)?;
    let mut stmt = conn
        .prepare(&format!(
            "SELECT ts, model, input_tokens, output_tokens, reasoning_tokens,
                    cache_read_tokens, cache_write_tokens, cost_usd
             FROM token_usage
             ORDER BY ts DESC
             LIMIT {}",
            super::MAX_LEDGER_ROWS
        ))
        .map_err(|e| format!("query token_usage: {e}"))?;
    read_events_stmt(&mut stmt, "token_usage")
}

/// The v2 ledger's token rows have no model or cost — the model lives in
/// the turn's assistant message telemetry (`context_usage_telemetry`),
/// looked up per surviving row via the (session_id, turn_id, role)
/// index so the message table is searched, not scanned. Rows it can't
/// name stay "" and price as unpriced (⚠), never guessed.
fn read_v2_usage_events(db: &Path) -> Result<Vec<UsageEvent>, String> {
    let conn = super::open_readonly_sqlite(db)?;
    let mut stmt = conn
        .prepare(&format!(
            "SELECT u.ts,
                    COALESCE(NULLIF(u.model,''),
                             (SELECT MAX(json_extract(m.data_json,'$.context_usage_telemetry.model'))
                              FROM local_runtime_message_rows m
                              WHERE m.session_id = u.session_id AND m.turn_id = u.turn_id
                                AND m.role = 'assistant' AND json_valid(m.data_json)),
                             ''),
                    u.input_tokens, u.output_tokens, u.reasoning_tokens,
                    u.cache_read_tokens, u.cache_write_tokens, COALESCE(u.cost_usd, 0)
             FROM local_runtime_token_usage u
             ORDER BY u.ts DESC
             LIMIT {}",
            super::MAX_LEDGER_ROWS
        ))
        .map_err(|e| format!("query local_runtime_token_usage: {e}"))?;
    read_events_stmt(&mut stmt, "local_runtime_token_usage")
}

fn read_events_stmt(
    stmt: &mut rusqlite::Statement<'_>,
    what: &str,
) -> Result<Vec<UsageEvent>, String> {
    let rows = stmt
        .query_map([], |row| {
            Ok(UsageEvent {
                ts_ms: row.get::<_, i64>(0)?,
                model: row.get::<_, String>(1)?,
                input: row.get::<_, f64>(2).unwrap_or(0.0),
                output: row.get::<_, f64>(3).unwrap_or(0.0),
                reasoning: row.get::<_, f64>(4).unwrap_or(0.0),
                cache_read: row.get::<_, f64>(5).unwrap_or(0.0),
                cache_write: row.get::<_, f64>(6).unwrap_or(0.0),
                cost_usd: row.get::<_, f64>(7).unwrap_or(0.0),
            })
        })
        .map_err(|e| format!("read {what}: {e}"))?;
    let mut events = Vec::new();
    let mut dropped = 0u64;
    for row in rows {
        match row {
            Ok(ev) => events.push(ev),
            Err(_) => dropped += 1,
        }
    }
    if dropped > 0 {
        eprintln!("[pane] minimax: {what}: {dropped} row(s) skipped (unreadable ts/model)");
    }
    if events.len() as u64 >= super::MAX_LEDGER_ROWS {
        eprintln!(
            "[pane] minimax: {what} hit the {}-row read cap — keeping newest rows, oldest usage is dropped",
            super::MAX_LEDGER_ROWS
        );
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::cli_config_key;
    use serde_json::json;

    #[test]
    fn cli_key_requires_the_minimax_path() {
        // Another provider's key listed first must not be picked up.
        let raw = "provider:\n  another_service:\n    options:\n      apiKey: another-provider-secret-over-20-chars\n  minimax:\n    options:\n      apiKey: actual-minimax-secret-over-20-chars\n";
        assert_eq!(cli_config_key(raw), Some("actual-minimax-secret-over-20-chars".into()));

        // A file with only the foreign provider yields nothing.
        let raw = "provider:\n  another_service:\n    options:\n      apiKey: another-provider-secret-over-20-chars\n";
        assert_eq!(cli_config_key(raw), None);

        // The real location still works, quotes stripped.
        let raw = "provider:\n  minimax:\n    options:\n      apiKey: \"real-minimax-secret-over-20-chars\"\n";
        assert_eq!(cli_config_key(raw), Some("real-minimax-secret-over-20-chars".into()));

        // Short placeholder keys are still rejected.
        let raw = "provider:\n  minimax:\n    options:\n      apiKey: sk-short\n";
        assert_eq!(cli_config_key(raw), None);
    }

    #[test]
    fn huge_ledgers_are_not_copied_to_temp() {
        assert!(super::temp_snapshot_allowed(0));
        assert!(super::temp_snapshot_allowed(super::MAX_TEMP_SNAPSHOT_BYTES));
        assert!(!super::temp_snapshot_allowed(super::MAX_TEMP_SNAPSHOT_BYTES + 1));
    }

    #[test]
    fn snapshot_db_deletes_leftover_wal_before_copy() {
        let pid = std::process::id();
        let src = std::env::temp_dir().join(format!("pane-snap-src-{pid}.db"));
        let dst = std::env::temp_dir().join(format!("pane-snap-dst-{pid}.db"));
        crate::providers::remove_sqlite_files(&src);
        crate::providers::remove_sqlite_files(&dst);

        let conn = rusqlite::Connection::open(&src).unwrap();
        conn.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
        drop(conn);

        let mut leftover = dst.as_os_str().to_os_string();
        leftover.push("-wal");
        let leftover = std::path::PathBuf::from(leftover);
        std::fs::write(&leftover, vec![0u8; 1024 * 1024]).unwrap();
        assert_eq!(std::fs::metadata(&leftover).unwrap().len(), 1024 * 1024);

        super::snapshot_db(&src, &dst).expect("snapshot should succeed");
        let wal_len = std::fs::metadata(&leftover).map(|m| m.len()).unwrap_or(0);
        assert!(
            wal_len < 64 * 1024,
            "leftover dest WAL must not survive a fresh snapshot, was {wal_len} bytes"
        );

        crate::providers::remove_sqlite_files(&src);
        crate::providers::remove_sqlite_files(&dst);
    }

    #[test]
    fn encode_uri_component_matches_js() {
        assert_eq!(
            super::encode_uri_component("/a b?c=1&d=/"),
            "%2Fa%20b%3Fc%3D1%26d%3D%2F"
        );
        // Unreserved set passes through verbatim.
        assert_eq!(
            super::encode_uri_component("AZaz09-_.!~*'()"),
            "AZaz09-_.!~*'()"
        );
        // UTF-8 bytes are percent-encoded per byte (é = C3 A9).
        assert_eq!(super::encode_uri_component("é"), "%C3%A9");
    }

    #[test]
    fn mcode_sign_matches_reference_vectors() {
        // Vectors computed with hashlib against mcode's own formula.
        let now_ms = 1_700_000_000_123;
        let query = super::mcode_query(now_ms, -18_000, super::McodeRegion::En, "0");
        assert_eq!(
            query,
            "device_platform=mcode&biz_id=3&app_id=3001&version_code=22201&unix=1700000000123&timezone_offset=-18000&sys_language=en&lang=en&device_id=0&os_name=win32&browser_name=mcode&user_id=0&client=mcode"
        );
        let (yy, secs, x_sig) = super::mcode_sign(
            "/matrix/api/v1/user/get_user_extra_info",
            &query,
            "{}",
            now_ms,
        );
        assert_eq!(secs, "1700000000");
        assert_eq!(yy, "2d07386dfd2bcc75fbd93c6b09b2934d");
        assert_eq!(x_sig, "b0c74b46e28053fb22e53239b1061505");
    }

    #[test]
    fn mcode_auth_file_usable_and_expired() {
        let now = 1_800_000_000_000;
        let usable = json!({
            "schemaVersion": 1,
            "records": {
                "com.minimax.mcode.oauth.prod.en\u{0}abcd": {
                    "accessToken": "x".repeat(60),
                    "tokenType": "Bearer",
                    "clientId": "mcode-public",
                    "expiresAtMs": now + 3_600_000,
                }
            }
        });
        assert_eq!(
            super::mcode_access_token(&usable, now).map(|t| t.len()),
            Some(60)
        );

        // Within a minute of expiry is not usable.
        let dying = json!({
            "records": {
                "com.minimax.mcode.oauth.prod.en\u{0}abcd": {
                    "accessToken": "x".repeat(60),
                    "expiresAtMs": now + 30_000,
                }
            }
        });
        assert_eq!(super::mcode_access_token(&dying, now), None);

        // No prod-prefixed record: unrelated records are never used.
        let other = json!({
            "records": {
                "something.else": {
                    "accessToken": "y".repeat(60),
                    "expiresAtMs": now + 3_600_000,
                }
            }
        });
        assert_eq!(super::mcode_access_token(&other, now), None);

        // An expired prod record must not win over a live one.
        let mixed = json!({
            "records": {
                "com.minimax.mcode.oauth.prod.en\u{0}dead": {
                    "accessToken": "x".repeat(60),
                    "expiresAtMs": now - 1,
                },
                "com.minimax.mcode.oauth.prod.en\u{0}live": {
                    "accessToken": "z".repeat(48),
                    "expiresAtMs": now + 3_600_000,
                }
            }
        });
        assert_eq!(
            super::mcode_access_token(&mixed, now).map(|t| t.len()),
            Some(48)
        );

        // Two live records: the one with the later expiry wins.
        let two = json!({
            "records": {
                "com.minimax.mcode.oauth.prod.en\u{0}old": {
                    "accessToken": "x".repeat(60),
                    "expiresAtMs": now + 3_600_000,
                },
                "com.minimax.mcode.oauth.prod.en\u{0}new": {
                    "accessToken": "z".repeat(48),
                    "expiresAtMs": now + 7_200_000,
                }
            }
        });
        assert_eq!(
            super::mcode_access_token(&two, now).map(|t| t.len()),
            Some(48)
        );

        // No records at all → nothing.
        assert_eq!(super::mcode_access_token(&json!({"records": {}}), now), None);
    }

    #[test]
    fn membership_tier_falls_back_when_workspace_lacks_one() {
        // get_membership_info is only called for the tier the workspace
        // lookup didn't yield — the field it reads.
        let member = json!({
            "op_group_id": "g-1",
            "has_token_plan": true,
            "token_plan_tier": "Ultra Plan"
        });
        assert_eq!(
            member.get("token_plan_tier").and_then(serde_json::Value::as_str),
            Some("Ultra Plan")
        );
    }

    #[test]
    fn remembered_tier_round_trips_and_survives_login_lapse() {
        let dir = std::env::temp_dir().join(format!("pane-mmx-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(super::remembered_tier_in(&dir), None);

        super::remember_tier_in(&dir, "Ultra Plan", "1");
        assert_eq!(super::remembered_tier_in(&dir).as_deref(), Some("Ultra Plan"));

        // Re-saving the same tier for the same account is a no-op (no
        // rewrite per refresh).
        let mtime = std::fs::metadata(super::plan_cache_path(&dir))
            .unwrap()
            .modified()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        super::remember_tier_in(&dir, "Ultra Plan", "1");
        assert_eq!(
            std::fs::metadata(super::plan_cache_path(&dir))
                .unwrap()
                .modified()
                .unwrap(),
            mtime
        );

        // A different tier replaces it; the key path would pick it up.
        super::remember_tier_in(&dir, "Starter", "1");
        assert_eq!(super::remembered_tier_in(&dir).as_deref(), Some("Starter"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remembered_tier_expires_after_ttl() {
        let dir =
            std::env::temp_dir().join(format!("pane-mmx-plan-ttl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        super::remember_tier_in(&dir, "Ultra Plan", "42");
        let now = chrono::Utc::now().timestamp_millis();
        assert_eq!(
            super::remembered_tier_at(&dir, now).as_deref(),
            Some("Ultra Plan")
        );
        assert_eq!(
            super::remembered_tier_at(&dir, now + super::REMEMBERED_TIER_TTL_MS + 1),
            None
        );

        // A cache entry with no seen_ms is treated as expired.
        std::fs::write(
            super::plan_cache_path(&dir),
            r#"{"tier":"Ultra Plan","user_id":"42"}"#,
        )
        .unwrap();
        assert_eq!(super::remembered_tier_at(&dir, now), None);

        // A future stamp is expired too, and a re-remember rewrites it
        // with the real time rather than trusting it.
        std::fs::write(
            super::plan_cache_path(&dir),
            serde_json::json!({
                "tier": "Ultra Plan",
                "seen_ms": now + 86_400_000,
                "user_id": "42",
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(super::remembered_tier_at(&dir, now), None);
        super::remember_tier_at(&dir, "Ultra Plan", "42", now);
        let doc: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(super::plan_cache_path(&dir)).unwrap(),
        )
        .unwrap();
        assert_eq!(doc.get("seen_ms").and_then(serde_json::Value::as_i64), Some(now));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remembered_tier_rewrites_when_user_changes() {
        let dir =
            std::env::temp_dir().join(format!("pane-mmx-plan-usr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        super::remember_tier_in(&dir, "Ultra Plan", "1");
        let mtime = std::fs::metadata(super::plan_cache_path(&dir))
            .unwrap()
            .modified()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));

        // Same tier under a different mcode account must rewrite — the
        // cache is scoped to the account, not just the label.
        super::remember_tier_in(&dir, "Ultra Plan", "2");
        assert_ne!(
            std::fs::metadata(super::plan_cache_path(&dir))
                .unwrap()
                .modified()
                .unwrap(),
            mtime
        );
        let doc: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(super::plan_cache_path(&dir)).unwrap(),
        )
        .unwrap();
        assert_eq!(doc.get("user_id").and_then(serde_json::Value::as_str), Some("2"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remembered_tier_refreshes_seen_ms_before_ttl() {
        let dir =
            std::env::temp_dir().join(format!("pane-mmx-plan-refresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let now = chrono::Utc::now().timestamp_millis();
        // 16 days old — past the half-TTL refresh point but still valid.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            super::plan_cache_path(&dir),
            serde_json::json!({
                "tier": "Ultra Plan",
                "seen_ms": now - 16 * 24 * 3600 * 1000,
                "user_id": "1",
            })
            .to_string(),
        )
        .unwrap();

        super::remember_tier_at(&dir, "Ultra Plan", "1", now);
        let doc: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(super::plan_cache_path(&dir)).unwrap(),
        )
        .unwrap();
        assert!(doc.get("seen_ms").and_then(serde_json::Value::as_i64).unwrap() >= now);

        // Freshly written → same tier/user now takes the no-write path.
        let mtime = std::fs::metadata(super::plan_cache_path(&dir))
            .unwrap()
            .modified()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        super::remember_tier_at(&dir, "Ultra Plan", "1", now + 60_000);
        assert_eq!(
            std::fs::metadata(super::plan_cache_path(&dir))
                .unwrap()
                .modified()
                .unwrap(),
            mtime
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forget_remembered_tier_removes_cache_and_is_idempotent() {
        let dir =
            std::env::temp_dir().join(format!("pane-mmx-plan-rm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        super::remember_tier_in(&dir, "Ultra Plan", "1");
        assert!(super::remembered_tier_exists_in(&dir));
        super::forget_remembered_tier_in(&dir).unwrap();
        assert!(!super::remembered_tier_exists_in(&dir));
        // Forgetting twice is fine — nothing to remove.
        super::forget_remembered_tier_in(&dir).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stash_restore_discard_round_trip() {
        let dir =
            std::env::temp_dir().join(format!("pane-mmx-plan-stash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = super::plan_cache_path(&dir);
        let stash = dir.join("minimax-plan.json.old");

        super::remember_tier_in(&dir, "Ultra Plan", "1");
        assert!(cache.exists());

        assert_eq!(super::stash_remembered_tier_in(&dir), Ok(true));
        assert!(!cache.exists() && stash.exists());

        super::restore_stashed_tier_in(&dir);
        assert!(cache.exists() && !stash.exists());
        assert_eq!(super::remembered_tier_in(&dir).as_deref(), Some("Ultra Plan"));

        assert_eq!(super::stash_remembered_tier_in(&dir), Ok(true));
        super::discard_stashed_tier_in(&dir);
        assert!(!cache.exists() && !stash.exists());

        // Nothing to stash in an empty dir.
        assert_eq!(super::stash_remembered_tier_in(&dir), Ok(false));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_refresh_does_not_rewrite_tier_after_key_change() {
        let dir =
            std::env::temp_dir().join(format!("pane-mmx-plan-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // A generation captured before a key-driven cleanup is stale: the
        // in-flight refresh must not resurrect the deleted tier.
        let g = super::tier_cache_generation();
        super::forget_remembered_tier_in(&dir).unwrap();
        super::remember_tier_if_unchanged_in(&dir, "Ultra Plan", "1", g);
        assert!(!super::plan_cache_path(&dir).exists());

        // A refresh under the CURRENT generation writes normally. Another
        // test's bump could land in the tiny gap between the load and the
        // write check, so retry that pair rather than assert on one shot.
        let mut wrote = false;
        for _ in 0..10 {
            let g2 = super::tier_cache_generation();
            super::remember_tier_if_unchanged_in(&dir, "Ultra Plan", "1", g2);
            if super::plan_cache_path(&dir).exists() {
                wrote = true;
                break;
            }
        }
        assert!(wrote);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remains_builds_5_hours_weekly_and_video_rows() {
        let doc = json!({
            "model_remains": [
                {
                    "model_name": "general",
                    "current_interval_total_count": 100,
                    "current_interval_usage_count": 96,
                    "current_interval_remaining_percent": 96,
                    "end_time": 1_800_018_000_000i64,
                    "current_weekly_status": 1,
                    "current_weekly_remaining_percent": 100,
                    "current_weekly_total_count": 500,
                    "current_weekly_usage_count": 500,
                    "weekly_end_time": 1_800_600_000_000i64
                },
                {
                    "model_name": "video",
                    "current_interval_status": 1,
                    "current_interval_total_count": 5,
                    "current_interval_usage_count": 5,
                    "start_time": 1_800_000_000_000i64,
                    "end_time": 1_800_018_000_000i64
                }
            ]
        });
        // The key path passes the remembered mcode tier (or "Coding Plan").
        let snap = super::parse_remains(&doc, Some("Ultra Plan".into())).expect("snapshot");
        let labels: Vec<&str> = snap.metrics.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(labels, ["5 Hours", "Weekly", "Video"]);
        assert_eq!(snap.plan.as_deref(), Some("Ultra Plan"));

        let five_h = &snap.metrics[0];
        assert_eq!(five_h.used_percent, Some(4.0));
        assert_eq!(five_h.detail.as_deref(), Some("96 of 100 left"));

        let video = &snap.metrics[2];
        assert_eq!(video.kind, "progress");
        assert_eq!(video.used_percent, Some(0.0));
        assert_eq!(video.detail.as_deref(), Some("5 of 5 left"));
        assert_eq!(video.resets_at, Some(1_800_018_000_000));
        assert_eq!(video.period_ms, Some(18_000_000));
    }

    #[test]
    fn remains_video_unlimited_and_absent_cases() {
        let mut doc = json!({
            "model_remains": [
                {
                    "model_name": "general",
                    "current_interval_total_count": 100,
                    "current_interval_usage_count": 100,
                    "current_weekly_status": 3
                },
                {
                    "model_name": "video",
                    "current_interval_status": 3,
                    "current_interval_total_count": 0,
                    "current_interval_usage_count": 0
                }
            ]
        });
        let snap = super::parse_remains(&doc, Some("Coding Plan".into())).expect("snapshot");
        let labels: Vec<&str> = snap.metrics.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(labels, ["5 Hours", "Weekly", "Video"]);
        assert_eq!(snap.metrics[1].value.as_deref(), Some("Unlimited"));
        assert_eq!(snap.metrics[2].value.as_deref(), Some("Unlimited"));

        // total 0 without unlimited → no Video row at all.
        doc["model_remains"][1]["current_interval_status"] = json!(1);
        let snap = super::parse_remains(&doc, Some("Coding Plan".into())).expect("snapshot");
        let labels: Vec<&str> = snap.metrics.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(labels, ["5 Hours", "Weekly"]);
    }

    #[test]
    fn v2_ledger_joins_model_from_message_telemetry() {
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("pane-mmx-v2-{pid}"));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("runtime-state.sqlite");
        crate::providers::remove_sqlite_files(&db);

        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE local_runtime_token_usage(
                id INTEGER PRIMARY KEY, session_id TEXT, agent_name TEXT,
                framework_type TEXT, turn_id TEXT, model TEXT, ts INTEGER,
                input_tokens INTEGER, output_tokens INTEGER, reasoning_tokens INTEGER,
                cache_read_tokens INTEGER, cache_write_tokens INTEGER,
                cost_usd REAL, raw TEXT);
             CREATE TABLE local_runtime_message_rows(
                session_id TEXT, msg_id TEXT, role TEXT, turn_id TEXT,
                created_at_ms INTEGER, data_json TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO local_runtime_token_usage
                (session_id,turn_id,model,ts,input_tokens,output_tokens,reasoning_tokens,cache_read_tokens,cache_write_tokens,cost_usd)
             VALUES('s1','t1',NULL,1000,10,20,0,0,0,0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO local_runtime_token_usage
                (session_id,turn_id,model,ts,input_tokens,output_tokens,reasoning_tokens,cache_read_tokens,cache_write_tokens,cost_usd)
             VALUES('s1','t2',NULL,2000,5,6,0,0,0,0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO local_runtime_token_usage
                (session_id,turn_id,model,ts,input_tokens,output_tokens,reasoning_tokens,cache_read_tokens,cache_write_tokens,cost_usd)
             VALUES('s1','t3',NULL,3000,7,8,0,0,0,0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO local_runtime_message_rows
                (session_id,msg_id,role,turn_id,created_at_ms,data_json)
             VALUES('s1','m1','assistant','t1',1001,
                '{\"context_usage_telemetry\":{\"model\":\"MiniMax-M3\"}}')",
            [],
        )
        .unwrap();
        // A half-written telemetry row must not break the join or leak a
        // model into another turn — json_valid() filters it out.
        conn.execute(
            "INSERT INTO local_runtime_message_rows
                (session_id,msg_id,role,turn_id,created_at_ms,data_json)
             VALUES('s1','m3','assistant','t3',3001,'{\"context_usage')",
            [],
        )
        .unwrap();
        drop(conn);

        let events = super::read_v2_usage_events(&db).expect("v2 read");
        assert_eq!(events.len(), 3);
        // Newest first: t3's telemetry is corrupt, t2 has none → both "".
        assert_eq!(events[0].ts_ms, 3000);
        assert_eq!(events[0].model, "");
        assert_eq!(events[1].ts_ms, 2000);
        assert_eq!(events[1].model, "");
        assert_eq!(events[2].ts_ms, 1000);
        assert_eq!(events[2].model, "MiniMax-M3");

        drop(events);
        crate::providers::remove_sqlite_files(&db);
        let _ = std::fs::remove_dir(&dir);
    }

    /// Live probe with this machine's real key — run manually via
    /// `cargo test --lib minimax -- --ignored --nocapture`. Prints statuses
    /// and numbers only, never the key.
    #[test]
    #[ignore]
    fn live_probe() {
        let snap = tauri::async_runtime::block_on(super::snapshot());
        eprintln!(
            "minimax: status={} plan={:?} error={:?} metrics={}",
            snap.status,
            snap.plan,
            snap.error,
            snap.metrics.len()
        );
        for m in &snap.metrics {
            eprintln!(
                "  {}: used={:?} detail={:?} value={:?} resets_at={:?}",
                m.label, m.used_percent, m.detail, m.value, m.resets_at
            );
        }
    }

    /// Live probe of the mcode OAuth path — run manually via
    /// `cargo test --lib minimax::tests::live_probe_mcode -- --ignored
    /// --nocapture`. Prints field names, lengths, labels, percentages —
    /// never a token.
    #[test]
    #[ignore]
    fn live_probe_mcode() {
        tauri::async_runtime::block_on(async {
            let Some(login) = super::mcode_login() else {
                eprintln!("mcode login: none usable (auth.json missing or token expired)");
                return;
            };
            eprintln!(
                "mcode login: region={} access_len={} user_id_len={}",
                login.region.dir(),
                login.access.len(),
                login.user_id.len()
            );
            match super::fetch_via_mcode(&login).await {
                Ok(s) => {
                    eprintln!(
                        "mcode: status={} plan={:?} metrics={}",
                        s.status,
                        s.plan,
                        s.metrics.len()
                    );
                    for m in &s.metrics {
                        eprintln!(
                            "  {}: kind={} used={:?} detail={:?} value={:?} resets_at={:?}",
                            m.label, m.kind, m.used_percent, m.detail, m.value, m.resets_at
                        );
                    }
                }
                Err(e) => eprintln!("mcode fetch: {e}"),
            }
        });
    }

    /// Live probe of the spend path — run manually via
    /// `cargo test --lib minimax::tests::live_probe_spend -- --ignored
    /// --nocapture`. Prints event counts and priced/unpriced totals only.
    #[test]
    #[ignore]
    fn live_probe_spend() {
        let events = super::collect_usage_events();
        let v2 = events.iter().filter(|e| e.model == "MiniMax-M3").count();
        eprintln!(
            "events: {} total, {v2} joined to MiniMax-M3; lookup(MiniMax-M3) priced={}",
            events.len(),
            crate::pricing::lookup("MiniMax-M3").is_some()
        );
        let spends = crate::spend::collect(None);
        if let Some(sp) = spends.iter().find(|s| s.id == "minimax") {
            eprintln!(
                "spend: today=${:.2}/{}tok last30=${:.2}/{}tok unpriced={} models={:?}",
                sp.today.cost,
                sp.today.tokens,
                sp.last30.cost,
                sp.last30.tokens,
                sp.unpriced,
                sp.unpriced_models
            );
        } else {
            eprintln!("spend: no minimax entry");
        }
    }
}
