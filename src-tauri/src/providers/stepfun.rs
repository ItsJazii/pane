use super::{bounded_text, config_value, http, json_body, stored_api_key, Metric, Snapshot};
use crate::spend;
use chrono::{Datelike, Local, NaiveDate};
use serde_json::Value;
use std::path::Path;

const ID: &str = "stepfun";
const NAME: &str = "StepFun";
// Wallet/probe JSON is kilobytes; 1 MiB bounds a hostile or broken body.
const MAX_BODY: usize = 1024 * 1024;

// Step Plan pricing rule (platform.stepfun.com/docs/zh/step-plan/overview):
// 1M Credit = ¥1, charged at model list price, pool issued monthly and
// cleared at month end. Our spend is USD and CN list ≈ USD × 7, so
// Credits(M) ≈ month USD × 7.
const CNY_PER_USD: f64 = 7.0;
const PLAN_TIERS: [(u64, &str); 4] = [
    (400, "Flash Mini"),
    (1600, "Flash Plus"),
    (8000, "Flash Pro"),
    (40000, "Flash Max"),
];

// One host per region, same paths — api.stepfun.com serves CN keys.
const ACCOUNT_URLS: [&str; 2] = [
    "https://api.stepfun.ai/v1/accounts",
    "https://api.stepfun.com/v1/accounts",
];
// Step Plan keys carry no wallet — the accounts endpoint rejects them.
// /models answers 200 for a valid Step Plan key, which is the only signal
// the API offers: plan quota itself is dashboard-only.
const PLAN_MODELS_URLS: [&str; 2] = [
    "https://api.stepfun.ai/step_plan/v1/models",
    "https://api.stepfun.com/step_plan/v1/models",
];

pub async fn snapshot() -> Snapshot {
    match fetch().await {
        Ok(s) => s,
        Err(e) => Snapshot::error(ID, NAME, e),
    }
}

/// Region hosts each carry a subset of keys, and any of them can be
/// down: advance to the next on auth rejection (401/403), rate limiting
/// (429), server errors (5xx), and transport failures. A 2xx from any
/// host is authoritative; a non-retry status (404-style) is a real
/// answer the callers interpret themselves.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Try {
    Ok,
    Status(u16),
    Transport,
}

/// What `try_urls` should do once every host answered.
#[derive(Debug, PartialEq)]
enum Pick {
    /// Return this host's response to the caller.
    Use(usize),
    /// Every host auth-rejected → the callers' "key rejected" path
    /// (plan probe, then the paste-a-key error).
    Rejected,
    /// Surface the last non-auth failure — a 5xx/429/transport error
    /// says more than the 401/403 another region returned.
    Fail(usize),
}

fn retryable(s: u16) -> bool {
    s == 401 || s == 403 || s == 429 || s >= 500
}

fn pick_outcome(outcomes: &[Try]) -> Pick {
    let mut last_non_auth = None;
    for (i, o) in outcomes.iter().enumerate() {
        match *o {
            Try::Ok => return Pick::Use(i),
            Try::Status(s) if !retryable(s) => return Pick::Use(i),
            Try::Status(s) => {
                if s != 401 && s != 403 {
                    last_non_auth = Some(i);
                }
            }
            Try::Transport => last_non_auth = Some(i),
        }
    }
    match last_non_auth {
        Some(i) => Pick::Fail(i),
        None => Pick::Rejected,
    }
}

/// GET `urls` in order with the key (.ai first, then .com). Returns the
/// answering url with the first non-retryable response, Err on the most
/// useful failure when every host fell over, or None when every host
/// auth-rejected the key.
async fn try_urls<'u>(
    urls: &[&'u str],
    key: &str,
) -> Result<Option<(&'u str, reqwest::Response)>, String> {
    let mut failures: Vec<(Try, String)> = Vec::new();
    for url in urls {
        match http().get(*url).bearer_auth(key).send().await {
            Err(e) => failures.push((Try::Transport, format!("{url}: {e}"))),
            Ok(resp) => {
                let s = resp.status().as_u16();
                if retryable(s) {
                    failures.push((Try::Status(s), format!("{url}: HTTP {s}")));
                } else {
                    return Ok(Some((*url, resp)));
                }
            }
        }
    }
    let outcomes: Vec<Try> = failures.iter().map(|(t, _)| *t).collect();
    match pick_outcome(&outcomes) {
        Pick::Fail(i) => Err(failures[i].1.clone()),
        // All auth rejections (Use is unreachable — non-retryable
        // statuses return inside the loop).
        Pick::Rejected | Pick::Use(_) => Ok(None),
    }
}

/// Step Code (StepFun's official CLI) stores its credential in
/// ~/.stepcode/auth.json as `{"step": {"type","access","profile",…}}`.
/// For `platform_*` profiles `access` is a plain StepFun API key — the
/// same key this card asks for — so a signed-in Step Code user needs
/// no Settings entry. `step_plan*` profiles hold a browser OAuth token
/// for the plan endpoint instead, which /v1/accounts can't use; those
/// are ignored, as are missing or garbled files. Never logged.
fn stepcode_api_key(auth_json: &str) -> Option<String> {
    let step = serde_json::from_str::<Value>(auth_json).ok()?.get("step")?.clone();
    let profile = step.get("profile").and_then(Value::as_str)?;
    if !profile.starts_with("platform_") {
        return None;
    }
    step.get("access")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string)
}

/// The Step Code credential file, if it holds a platform API key.
fn stepcode_key() -> Option<String> {
    let path = dirs::home_dir()
        .unwrap_or_default()
        .join(".stepcode")
        .join("auth.json");
    // auth.json is a few hundred bytes — size-gate before reading so a
    // swapped or corrupt file can't be slurped wholesale.
    let meta = std::fs::metadata(&path).ok()?;
    if meta.len() > 64 * 1024 {
        return None;
    }
    let raw = std::fs::read_to_string(&path).ok()?;
    stepcode_api_key(&raw)
}

async fn fetch() -> Result<Snapshot, String> {
    let Some(key) = stored_api_key("stepfun", &["STEPFUN_API_KEY", "STEP_API_KEY"])
        .or_else(stepcode_key)
    else {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Paste a StepFun API key in Settings (gear icon).",
        ));
    };

    match try_urls(&ACCOUNT_URLS, &key).await? {
        Some((url, resp)) => {
            if !resp.status().is_success() {
                return Err(format!("accounts endpoint: HTTP {}", resp.status()));
            }
            let doc = json_body(resp, MAX_BODY, "accounts").await?;
            // .com accounts are billed in CNY.
            let sign = if url.contains("stepfun.com") { "¥" } else { "$" };
            // A Step Plan subscription can't be detected over the API —
            // /step_plan/v1/models answers 200 for any valid key, and
            // probing a real plan endpoint would spend the user's money.
            // So the Plan Credits bar is opt-in: it appears only when the
            // user picked a tier in Settings. With a tier the card shows
            // both pots — wallet rows first (its bar is labeled "Wallet";
            // pay-as-you-go clients on /v1 drain it), the Plan Credits
            // estimate last.
            let tier = plan_tier();
            let (plan, mut metrics) = parse_account(
                &doc,
                sign,
                Some(if tier.is_some() { "Wallet" } else { "Credits used" }),
            )?;
            if tier.is_none() {
                return Ok(Snapshot::ok(ID, NAME, plan, metrics));
            }
            let (chip, plan_rows, warning) =
                plan_metrics(spend::month_to_date_cost(ID), tier);
            metrics.extend(plan_rows);
            let mut snap = Snapshot::ok(ID, NAME, chip, metrics);
            snap.warning = warning;
            Ok(snap)
        }
        None => {
            // Both wallet endpoints rejected the key — it may be a Step
            // Plan key, which only exists on the plan surface. A 200 on
            // /models proves the key is real rather than a typo.
            let plan_probe = try_urls(&PLAN_MODELS_URLS, &key).await?;
            if let Some((_, resp)) = plan_probe {
                if !resp.status().is_success() {
                    return Err("key was rejected — paste a fresh key in Settings (gear icon)".into());
                }
                // The probe only needs the status, but still bound the
                // body — no unbounded reads anywhere on this card.
                let _ = bounded_text(resp, MAX_BODY).await;
                let (chip, metrics, warning) =
                    plan_metrics(spend::month_to_date_cost(ID), plan_tier());
                let mut snap = Snapshot::ok(ID, NAME, chip, metrics);
                snap.warning = warning;
                return Ok(snap);
            }
            Err("key was rejected — paste a fresh key in Settings (gear icon)".into())
        }
    }
}

/// The user's Step Plan tier pick (Settings → API keys → StepFun) matched
/// against the official monthly pools. Anything else — unset, edited by
/// hand, a tier we don't know — is "no bar".
fn plan_tier() -> Option<(u64, &'static str)> {
    let credits = config_value("stepfunPlanCredits")?.as_u64()?;
    PLAN_TIERS.iter().copied().find(|(c, _)| *c == credits)
}

/// `n` with thousands separators, e.g. `40000` → `"40,000"`.
fn grouped(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.char_indices() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// (reset instant, window length) in epoch ms for the plan's monthly pool:
/// 00:00 local on the 1st of next month minus the 1st of this month.
fn month_window_ms() -> Option<(i64, i64)> {
    let first = Local::now().date_naive().with_day(1)?;
    let (y, m) = if first.month() == 12 {
        (first.year() + 1, 1)
    } else {
        (first.year(), first.month() + 1)
    };
    let next = NaiveDate::from_ymd_opt(y, m, 1)?;
    let ms = |d: NaiveDate| {
        d.and_hms_opt(0, 0, 0)?
            .and_local_timezone(Local)
            .single()
            .map(|t| t.timestamp_millis())
    };
    let (start, end) = (ms(first)?, ms(next)?);
    Some((end, end - start))
}

/// Step Plan rows: the "Plan Credits" estimate from this month's local
/// spend, as a bar against the configured tier. Chip is the tier name.
/// The `None` tier branch only runs for a plan-only key — one the
/// wallet endpoints rejected outright. With no tier picked there is no
/// bar at all (0% of an unknown pool would lie); the estimate stays a
/// text row and a visible "Plan tier" row carries the pick-a-tier hint —
/// Snapshot.warning never renders on an ok card.
fn plan_metrics(
    month_cost_usd: Option<f64>,
    tier: Option<(u64, &'static str)>,
) -> (Option<String>, Vec<Metric>, Option<String>) {
    let chip = Some(tier.map(|(_, n)| n).unwrap_or("Step Plan").to_string());
    let Some((limit, _)) = tier else {
        let estimate = month_cost_usd
            .map(|usd| {
                format!("≈{:.0}M Credits this month · est. from logs", usd * CNY_PER_USD)
            })
            .unwrap_or_else(|| "Estimating…".into());
        return (
            chip,
            vec![
                Metric::text("Plan Credits", estimate),
                Metric::text(
                    "Plan tier",
                    "Not set — pick one in Settings → API keys → StepFun".into(),
                ),
            ],
            None,
        );
    };
    let Some(usd) = month_cost_usd else {
        return (
            chip,
            vec![Metric::progress(
                "Plan Credits",
                0.0,
                // The frontend keys on this exact string to re-fetch once
                // the first spend scan lands.
                Some("Estimating from session logs…".into()),
            )],
            None,
        );
    };
    let used_m = usd * CNY_PER_USD;
    let pct = (used_m / limit as f64 * 100.0).clamp(0.0, 100.0);
    let (resets_at, period_ms) = month_window_ms()
        .map(|(r, p)| (Some(r), Some(p)))
        .unwrap_or((None, None));
    let metric = Metric::progress(
        "Plan Credits",
        pct,
        Some(format!(
            "≈{used_m:.0}M of {}M Credits used · est. from logs",
            grouped(limit)
        )),
    )
    .with_reset(resets_at, period_ms);
    (chip, vec![metric], None)
}

/// `GET /v1/accounts` body: `{object:"account", type:"prepaid"|"postpaid",
/// balance, total_cash_balance, total_voucher_balance}`. `sign` is the
/// region's currency ($ on .ai, ¥ on .com). `meter_label` is the wallet
/// bar's row label — `Some("Credits used")` for a plain wallet key,
/// `Some("Wallet")` when a Step Plan sits beside it (pay-as-you-go
/// clients on /v1 still drain the wallet), `None` for no meter at all.
fn parse_account(
    doc: &Value,
    sign: &str,
    meter_label: Option<&str>,
) -> Result<(Option<String>, Vec<Metric>), String> {
    parse_account_in(&super::config_dir(), doc, sign, meter_label)
}

/// `parse_account` against a caller-chosen config dir so tests never
/// touch the real `credit_baselines.json` high-water marks.
fn parse_account_in(
    dir: &Path,
    doc: &Value,
    sign: &str,
    meter_label: Option<&str>,
) -> Result<(Option<String>, Vec<Metric>), String> {
    let balance = doc
        .get("balance")
        .and_then(Value::as_f64)
        .ok_or_else(|| "account response has no balance".to_string())?;

    let mut metrics = Vec::new();
    // Credits-used meter against the highest balance seen locally —
    // top-ups raise it (feeds the Almost Out notification).
    if let Some(label) = meter_label {
        if let Some(meter) = super::credit_meter_labeled_in(dir, ID, sign, balance, label, "") {
            metrics.push(meter);
        }
    }
    metrics.push(Metric::text("Balance", format!("{sign}{balance:.2}")));
    let vouchers = doc
        .get("total_voucher_balance")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if vouchers > 0.0 {
        metrics.push(Metric::text("Vouchers", format!("{sign}{vouchers:.2}")));
    }

    let plan = doc.get("type").and_then(Value::as_str).and_then(|t| match t {
        "prepaid" => Some("Prepaid".to_string()),
        "postpaid" => Some("Postpaid".to_string()),
        _ => None,
    });
    Ok((plan, metrics))
}

#[cfg(test)]
mod tests {
    use super::{parse_account_in, pick_outcome, plan_metrics, stepcode_api_key, Metric, Pick, Try};
    use serde_json::json;
    use std::path::PathBuf;

    fn text_row<'a>(metrics: &'a [Metric], label: &str) -> Option<&'a Metric> {
        metrics.iter().find(|m| m.kind == "text" && m.label == label)
    }

    // A fresh dir per test: credit_meter_labeled_in persists
    // credit_baselines.json, and the real config dir's high-water marks
    // must never see test balances.
    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pane-stepfun-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn prepaid_with_vouchers_shows_plan_balance_and_vouchers() {
        let dir = tmpdir("prepaid");
        let (plan, metrics) = parse_account_in(&dir, &json!({
            "object": "account",
            "type": "prepaid",
            "balance": 12.345,
            "total_cash_balance": 10.0,
            "total_voucher_balance": 2.345,
        }), "$", Some("Credits used"))
        .unwrap();
        assert_eq!(plan.as_deref(), Some("Prepaid"));
        assert_eq!(
            text_row(&metrics, "Balance").and_then(|m| m.value.as_deref()),
            Some("$12.35")
        );
        assert_eq!(
            text_row(&metrics, "Vouchers").and_then(|m| m.value.as_deref()),
            Some("$2.35")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cn_accounts_show_yuan() {
        // .com accounts bill in CNY — same body, ¥ sign.
        let dir = tmpdir("cn");
        let (plan, metrics) = parse_account_in(&dir, &json!({
            "object": "account",
            "type": "prepaid",
            "balance": 99.08,
            "total_cash_balance": 100.0,
            "total_voucher_balance": 0.0,
        }), "¥", Some("Credits used"))
        .unwrap();
        assert_eq!(plan.as_deref(), Some("Prepaid"));
        assert_eq!(
            text_row(&metrics, "Balance").and_then(|m| m.value.as_deref()),
            Some("¥99.08")
        );
        assert!(text_row(&metrics, "Vouchers").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn postpaid_without_vouchers_hides_the_voucher_row() {
        let dir = tmpdir("postpaid");
        let (plan, metrics) = parse_account_in(&dir, &json!({
            "object": "account",
            "type": "postpaid",
            "balance": 4.0,
            "total_cash_balance": 4.0,
            "total_voucher_balance": 0.0,
        }), "$", Some("Credits used"))
        .unwrap();
        assert_eq!(plan.as_deref(), Some("Postpaid"));
        assert_eq!(
            text_row(&metrics, "Balance").and_then(|m| m.value.as_deref()),
            Some("$4.00")
        );
        assert!(text_row(&metrics, "Vouchers").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_key_keeps_a_wallet_bar_beside_balance() {
        // omp-style clients bill the pay-as-you-go wallet directly via
        // /v1 — a Step Plan key still needs the API-authoritative meter.
        let dir = tmpdir("wallet-bar");
        let (plan, metrics) = parse_account_in(&dir, &json!({
            "object": "account",
            "type": "prepaid",
            "balance": 43.40,
            "total_cash_balance": 43.40,
            "total_voucher_balance": 0.0,
        }), "¥", Some("Wallet"))
        .unwrap();
        assert_eq!(plan.as_deref(), Some("Prepaid"));
        assert_eq!(metrics[0].kind, "progress");
        assert_eq!(metrics[0].label, "Wallet");
        assert!(
            metrics[0].detail.as_deref().is_some_and(|d| d.contains("¥43.40")),
            "detail={:?}", metrics[0].detail
        );
        assert_eq!(
            text_row(&metrics, "Balance").and_then(|m| m.value.as_deref()),
            Some("¥43.40")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_balance_is_an_error() {
        let dir = tmpdir("missing");
        assert!(
            parse_account_in(&dir, &json!({"object": "account", "type": "prepaid"}), "$", Some("Credits used")).is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_metrics_with_tier_is_a_monthly_bar() {
        let (chip, metrics, warning) =
            plan_metrics(Some(4.35), Some((1600, "Flash Plus")));
        assert_eq!(chip.as_deref(), Some("Flash Plus"));
        assert!(warning.is_none());
        assert_eq!(metrics.len(), 1);
        let m = &metrics[0];
        assert_eq!(m.label, "Plan Credits");
        assert_eq!(m.kind, "progress");
        // $4.35 × 7 ≈ 30.45M of 1,600M → ~1.9%.
        let pct = m.used_percent.unwrap();
        assert!((pct - 1.9).abs() < 0.1, "pct={pct}");
        assert!(
            m.detail.as_deref().is_some_and(|d| d.starts_with("≈30M of 1,600M")),
            "detail={:?}", m.detail
        );
        assert!(m.resets_at.is_some());
        // A calendar month is always longer than 27 days.
        assert!(m.period_ms.is_some_and(|p| p > 27 * 24 * 3_600_000));
    }

    #[test]
    fn plan_metrics_without_tier_is_text_plus_tier_hint() {
        let (chip, metrics, warning) = plan_metrics(Some(4.35), None);
        assert_eq!(chip.as_deref(), Some("Step Plan"));
        // The hint rides a visible row now — Snapshot.warning never
        // renders on an ok card.
        assert!(warning.is_none());
        assert_eq!(metrics.len(), 2);
        let m = &metrics[0];
        assert_eq!(m.kind, "text");
        assert_eq!(m.label, "Plan Credits");
        assert!(
            m.value.as_deref().is_some_and(|v| v.contains("≈30M Credits")),
            "value={:?}", m.value
        );
        let hint = &metrics[1];
        assert_eq!(hint.kind, "text");
        assert_eq!(hint.label, "Plan tier");
        assert!(
            hint.value.as_deref().is_some_and(|v| v.starts_with("Not set")),
            "value={:?}", hint.value
        );
    }

    #[test]
    fn plan_metrics_without_tier_never_shows_a_bar() {
        // Even before the first spend scan: no 0% progress bar against
        // an unknown pool.
        let (_, metrics, _) = plan_metrics(None, None);
        assert_eq!(metrics.len(), 2);
        assert_eq!(metrics[0].kind, "text");
        assert_eq!(metrics[0].label, "Plan Credits");
        assert_eq!(metrics[0].value.as_deref(), Some("Estimating…"));
        assert_eq!(metrics[1].label, "Plan tier");
    }

    #[test]
    fn plan_metrics_before_first_scan_estimates() {
        let (_, metrics, warning) = plan_metrics(None, Some((8000, "Flash Pro")));
        assert!(warning.is_none());
        let m = &metrics[0];
        assert_eq!(m.kind, "progress");
        assert_eq!(m.used_percent, Some(0.0));
        assert_eq!(m.detail.as_deref(), Some("Estimating from session logs…"));
    }

    #[test]
    fn plan_metrics_clamps_over_the_pool() {
        let (_, metrics, _) = plan_metrics(Some(1000.0), Some((400, "Flash Mini")));
        assert_eq!(metrics[0].used_percent, Some(100.0));
    }

    /// Step Code's auth.json only lends its key for platform_*
    /// profiles — a step_plan OAuth token can't call /v1/accounts.
    #[test]
    fn stepcode_key_only_for_platform_profiles() {
        let platform = r#"{"step": {"type": "api_key", "access": "sk-test-123",
            "refresh": "r", "expires": 0, "profile": "platform_oversea"}}"#;
        assert_eq!(stepcode_api_key(platform).as_deref(), Some("sk-test-123"));
        let cn = platform.replace("platform_oversea", "platform_cn");
        assert_eq!(stepcode_api_key(&cn).as_deref(), Some("sk-test-123"));
        // Step Plan profiles carry an OAuth token, not an API key.
        let plan = platform.replace("platform_oversea", "step_plan_oversea");
        assert_eq!(stepcode_api_key(&plan), None);
        let plan2 = platform.replace("platform_oversea", "step_plan");
        assert_eq!(stepcode_api_key(&plan2), None);
        // Missing step/access, empty access, garbled JSON → nothing.
        assert_eq!(stepcode_api_key(r#"{"other": {}}"#), None);
        assert_eq!(stepcode_api_key(r#"{"step": {"profile": "platform_cn"}}"#), None);
        assert_eq!(
            stepcode_api_key(r#"{"step": {"profile": "platform_cn", "access": "  "}}"#),
            None
        );
        assert_eq!(stepcode_api_key("not json"), None);
    }

    #[test]
    fn pick_outcome_uses_the_first_real_answer() {
        // A down/limited/rejecting host falls through to the next; any
        // real answer (2xx or a non-retryable status) wins immediately.
        assert_eq!(pick_outcome(&[Try::Status(503), Try::Ok]), Pick::Use(1));
        assert_eq!(pick_outcome(&[Try::Status(401), Try::Ok]), Pick::Use(1));
        assert_eq!(pick_outcome(&[Try::Transport, Try::Ok]), Pick::Use(1));
        assert_eq!(pick_outcome(&[Try::Status(403), Try::Status(404)]), Pick::Use(1));
        assert_eq!(pick_outcome(&[Try::Ok, Try::Status(503)]), Pick::Use(0));
        assert_eq!(pick_outcome(&[Try::Status(429), Try::Ok]), Pick::Use(1));
    }

    #[test]
    fn pick_outcome_prefers_a_non_auth_error() {
        // All hosts down: a 5xx/429/transport says more than another
        // region's 401. All-auth stays Rejected → the callers' None
        // path (plan probe / paste-a-key error).
        assert_eq!(
            pick_outcome(&[Try::Status(503), Try::Status(401)]),
            Pick::Fail(0)
        );
        assert_eq!(
            pick_outcome(&[Try::Status(401), Try::Transport]),
            Pick::Fail(1)
        );
        assert_eq!(
            pick_outcome(&[Try::Status(401), Try::Status(429), Try::Status(500)]),
            Pick::Fail(2)
        );
        assert_eq!(
            pick_outcome(&[Try::Status(401), Try::Status(401)]),
            Pick::Rejected
        );
        assert_eq!(
            pick_outcome(&[Try::Status(401), Try::Status(403)]),
            Pick::Rejected
        );
    }
}
