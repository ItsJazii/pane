//! Pace-based notification rules, mirroring the Mac app:
//! - "Almost Out" — a metric drops under 10% remaining.
//! - "Cutting It Close" — projected to finish the period with <10% spare.
//! - "Will Run Out" — projected to hit the limit before the reset.
//!
//! Anti-spam: an alert fires only when a quota *worsens while the app is
//! running* (the first reading after launch is a silent baseline), fires
//! once per state, re-arms if the metric recovers, and the slate is wiped
//! when a new reset period begins. State is in-memory by design — matching
//! the Mac's "already-bad at launch won't alert" behavior.

use crate::providers::Snapshot;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

#[derive(Default, Clone)]
struct MetricState {
    resets_at: Option<i64>,
    seen: bool,
    almost_out: bool,
    close: bool,
    run_out: bool,
}

fn states() -> &'static Mutex<HashMap<String, MetricState>> {
    static STATES: OnceLock<Mutex<HashMap<String, MetricState>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub struct Alert {
    pub title: String,
    pub body: String,
}

#[derive(PartialEq, Clone, Copy)]
enum Verdict {
    Ok,
    Close,
    RunOut,
}

/// Same straight-line projection the UI uses for bar colors.
fn verdict(used: f64, resets_at: Option<i64>, period_ms: Option<i64>) -> (Verdict, f64) {
    let used = used.clamp(0.0, 100.0);
    let left = 100.0 - used;
    if left < 0.5 {
        return (Verdict::RunOut, 0.0);
    }
    let (Some(resets_at), Some(period_ms)) = (resets_at, period_ms) else {
        return (Verdict::Ok, left);
    };
    if period_ms <= 0 {
        return (Verdict::Ok, left);
    }
    let now = chrono::Utc::now().timestamp_millis();
    let remain = (resets_at - now).max(0);
    let elapsed = period_ms - remain;
    let frac = elapsed as f64 / period_ms as f64;
    if frac < 0.05 || elapsed < 5 * 60_000 {
        return (Verdict::Ok, left);
    }
    let projected = used / frac;
    let spare = (100.0 - projected).max(0.0);
    if projected >= 100.0 {
        (Verdict::RunOut, 0.0)
    } else if projected >= 90.0 {
        (Verdict::Close, spare.max(1.0))
    } else {
        (Verdict::Ok, spare)
    }
}

/// A reset time that moved by more than ten minutes means a new period
/// (small drifts happen because some providers report "seconds from now").
fn period_changed(old: Option<i64>, new: Option<i64>) -> bool {
    match (old, new) {
        (Some(a), Some(b)) => (a - b).abs() > 10 * 60_000,
        _ => false,
    }
}

pub fn evaluate(snapshots: &[Snapshot], cfg: &Value) -> Vec<Alert> {
    let want = |key: &str| cfg.get(key).and_then(Value::as_bool).unwrap_or(false);
    let want_almost = want("notifyAlmostOut");
    let want_close = want("notifyCuttingClose");
    let want_runout = want("notifyWillRunOut");
    if !(want_almost || want_close || want_runout) {
        return Vec::new();
    }

    let mut alerts = Vec::new();
    let Ok(mut map) = states().lock() else { return alerts };

    for snapshot in snapshots.iter().filter(|s| s.status == "ok") {
        if snapshot.id.starts_with("sub2api@") {
            let disabled = cfg.get("disabled").and_then(Value::as_array);
            if snapshot.stale || disabled.is_some_and(|ids| ids.iter().any(|id| {
                id.as_str().is_some_and(|id| id == "sub2api" || id == snapshot.id)
            })) {
                continue;
            }
        }
        for metric in snapshot.metrics.iter().filter(|m| m.kind == "progress") {
            // Restored Kimi API rows are last-known, not live — don't
            // fire Almost Out off a wallet timeout.
            if snapshot.id == "kimi"
                && snapshot.warning.is_some()
                && matches!(metric.label.as_str(), "API" | "Credits used")
            {
                continue;
            }
            let Some(used) = metric.used_percent else { continue };
            if !used.is_finite() || used < 0.0 {
                continue;
            }
            let key = format!("{}:{}", snapshot.id, metric.label);
            let entry = map.entry(key).or_default();

            if period_changed(entry.resets_at, metric.resets_at) {
                *entry = MetricState::default();
            }
            entry.resets_at = metric.resets_at;

            let left = (100.0 - used.clamp(0.0, 100.0)).max(0.0);
            let (v, spare) = verdict(used, metric.resets_at, metric.period_ms);
            let almost_now = left < 10.0;
            let close_now = v == Verdict::Close;
            let run_out_now = v == Verdict::RunOut;
            let baseline = !entry.seen;
            entry.seen = true;

            if !baseline {
                let shown = crate::i18n::metric_label(cfg, &metric.label);
                let name = format!("{} {}", snapshot.name, shown);
                let loc = crate::i18n::resolved_locale(cfg);
                if want_runout && run_out_now && !entry.run_out {
                    alerts.push(Alert {
                        title: match loc {
                            "zh" => "将会用完".into(),
                            "ru" => "Кончится до сброса".into(),
                            _ => "Will Run Out".into(),
                        },
                        body: match loc {
                            "zh" => format!("{name} 按当前速度会在重置前用完。"),
                            "ru" => format!("{name} при текущем темпе исчерпается до сброса."),
                            _ => format!("{name} is on pace to hit its limit before the reset."),
                        },
                    });
                } else if want_close && close_now && !entry.close {
                    alerts.push(Alert {
                        title: match loc {
                            "zh" => "余量紧张".into(),
                            "ru" => "Запас на исходе".into(),
                            _ => "Cutting It Close".into(),
                        },
                        body: match loc {
                            "zh" => format!("{name} 按当前速度重置时大约只剩 {spare:.0}%。"),
                            "ru" => format!("{name} к сбросу останется примерно {spare:.0}%."),
                            _ => format!(
                                "{name} is on pace to finish with only ~{spare:.0}% spare."
                            ),
                        },
                    });
                }
                if want_almost && almost_now && !entry.almost_out {
                    alerts.push(Alert {
                        title: match loc {
                            "zh" => "即将用完".into(),
                            "ru" => "Почти кончилось".into(),
                            _ => "Almost Out".into(),
                        },
                        body: match loc {
                            "zh" => format!("{name} 剩余不足 10%（还剩 {left:.0}%）。"),
                            "ru" => format!("{name} осталось меньше 10% (ещё {left:.0}%)."),
                            _ => format!("{name} is under 10% remaining ({left:.0}% left)."),
                        },
                    });
                }
            }

            entry.almost_out = almost_now;
            entry.close = close_now;
            entry.run_out = run_out_now;
        }
    }
    alerts
}

/// Drop every metric keyed as `{snapshot_id}:…`. Prefix is `id + ':'` so
/// `onenewapi@abc` does not also wipe `onenewapi@abcd`.
pub fn forget_snapshot(id: &str) {
    let prefix = format!("{id}:");
    let Ok(mut map) = states().lock() else {
        return;
    };
    map.retain(|k, _| !k.starts_with(&prefix));
}

#[cfg(test)]
pub fn insert_state_for_test(key: &str) {
    let Ok(mut map) = states().lock() else {
        return;
    };
    map.insert(key.to_string(), MetricState::default());
}

#[cfg(test)]
pub fn has_state_for_test(key: &str) -> bool {
    states()
        .lock()
        .map(|map| map.contains_key(key))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub2api_alerts_ignore_stale_disabled_and_nonfinite_observations() {
        let id = "sub2api@alert-eligibility";
        let cfg = serde_json::json!({"notifyAlmostOut": true, "locale": "en"});
        let make = |used| Snapshot::ok(id, "Site · Key", None,
            vec![crate::providers::Metric::progress("Total quota", used, None)]);
        forget_snapshot(id);
        assert!(evaluate(&[make(50.0)], &cfg).is_empty());
        let mut stale = make(95.0);
        stale.stale = true;
        assert!(evaluate(&[stale], &cfg).is_empty());
        assert!(evaluate(&[make(f64::NAN)], &cfg).is_empty());
        for disabled in ["sub2api", id] {
            let mut off = cfg.clone();
            off["disabled"] = serde_json::json!([disabled]);
            assert!(evaluate(&[make(95.0)], &off).is_empty());
        }
        assert_eq!(evaluate(&[make(95.0)], &cfg).len(), 1);
        forget_snapshot(id);
    }

    #[test]
    fn sub2api_thresholds_are_independent_for_equal_keys_and_each_allowance() {
        let ids = ["sub2api@alert-a", "sub2api@alert-b"];
        let cfg = serde_json::json!({"notifyAlmostOut": true, "locale": "en"});
        let make = |id: &str, used| Snapshot::ok(id, id, None, vec![
            crate::providers::Metric::progress("5h", used, None),
            crate::providers::Metric::progress("1d", used, None),
        ]);
        for id in ids { forget_snapshot(id); }
        assert!(evaluate(&[make(ids[0], 40.0), make(ids[1], 40.0)], &cfg).is_empty());
        let alerts = evaluate(&[make(ids[0], 95.0), make(ids[1], 95.0)], &cfg);
        assert_eq!(alerts.len(), 4);
        assert_eq!(alerts.iter().filter(|alert| alert.body.contains(ids[0])).count(), 2);
        assert!(evaluate(&[make(ids[0], 95.0), make(ids[1], 95.0)], &cfg).is_empty());
        let wallet = Snapshot::ok(ids[0], "Wallet", None,
            vec![crate::providers::Metric::text("Balance", "$-20.00".into())]);
        assert!(evaluate(&[wallet], &cfg).is_empty());
        for id in ids { forget_snapshot(id); }
    }

    #[test]
    fn forget_snapshot_drops_that_id_only() {
        insert_state_for_test("onenewapi@ticket07-abc:Usage");
        insert_state_for_test("onenewapi@ticket07-abc:Expiry");
        insert_state_for_test("onenewapi@ticket07-abcd:Usage");
        forget_snapshot("onenewapi@ticket07-abc");
        assert!(!has_state_for_test("onenewapi@ticket07-abc:Usage"));
        assert!(!has_state_for_test("onenewapi@ticket07-abc:Expiry"));
        assert!(has_state_for_test("onenewapi@ticket07-abcd:Usage"));
        forget_snapshot("onenewapi@ticket07-abcd");
    }
}
