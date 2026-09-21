use super::{config_value, http, stored_api_key, Metric, Snapshot};
use crate::spend;
use chrono::{Datelike, Local, NaiveDate};
use serde_json::Value;
use std::path::Path;

const ID: &str = "stepfun";
const NAME: &str = "StepFun";

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

/// GET `urls` in order with the key; a 401 falls through to the next host
/// (a key is valid on one region only). Returns the answering url with
/// the first non-401 response, or None when every host rejected the key.
async fn try_urls<'u>(
    urls: &[&'u str],
    key: &str,
) -> Result<Option<(&'u str, reqwest::Response)>, String> {
    for url in urls {
        let resp = http()
            .get(*url)
            .bearer_auth(key)
            .send()
            .await
            .map_err(|e| format!("{url}: {e}"))?;
        if resp.status().as_u16() != 401 {
            return Ok(Some((*url, resp)));
        }
    }
    Ok(None)
}

async fn fetch() -> Result<Snapshot, String> {
    let Some(key) = stored_api_key("stepfun", &["STEPFUN_API_KEY", "STEP_API_KEY"]) else {
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
            let doc: Value = resp.json().await.map_err(|e| format!("accounts parse: {e}"))?;
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
            if plan_probe.is_some_and(|(_, r)| r.status().is_success()) {
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
/// The `None` tier branch (text row + pick-a-tier hint) only runs for a
/// plan-only key — one the wallet endpoints rejected outright.
fn plan_metrics(
    month_cost_usd: Option<f64>,
    tier: Option<(u64, &'static str)>,
) -> (Option<String>, Vec<Metric>, Option<String>) {
    let chip = Some(tier.map(|(_, n)| n).unwrap_or("Step Plan").to_string());
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
    match tier {
        Some((limit, _)) => {
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
        None => (
            chip,
            vec![Metric::text(
                "Plan Credits",
                format!("≈{used_m:.0}M Credits this month · est. from logs"),
            )],
            Some(
                "Pick your Step Plan tier in Settings → API keys → StepFun to \
                 turn Credits into a bar (StepFun's API doesn't report plan \
                 quota)."
                    .into(),
            ),
        ),
    }
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
    use super::{parse_account_in, plan_metrics, Metric};
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
    fn plan_metrics_without_tier_is_text_plus_warning() {
        let (chip, metrics, warning) = plan_metrics(Some(4.35), None);
        assert_eq!(chip.as_deref(), Some("Step Plan"));
        assert!(warning.is_some());
        let m = &metrics[0];
        assert_eq!(m.kind, "text");
        assert_eq!(m.label, "Plan Credits");
        assert!(
            m.value.as_deref().is_some_and(|v| v.contains("≈30M Credits")),
            "value={:?}", m.value
        );
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
}
