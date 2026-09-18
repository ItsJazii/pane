//! Pace-based notification rules, mirroring the Mac app:
//! - "Almost Out" — a metric drops under 10% remaining.
//! - "Cutting It Close" — projected to finish the period with <10% spare.
//! - "Will Run Out" — projected to hit the limit before the reset.
//! - "Limit Reset" — a weekly-or-longer reset window rolled over and the
//!   quota is full (short windows reset too often to be worth a toast).
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
    prev_used: Option<f64>,
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

/// Reset toasts only cover weekly-and-longer windows — 5-hour and daily
/// windows roll too often to be worth a notification.
const LONG_WINDOW_MS: i64 = 6 * 24 * 3_600_000;

/// Is this metric's window long enough to toast about? The declared
/// `period_ms` wins; when a provider reports no period, the jump between
/// consecutive reset times IS the period.
fn long_window(
    period_ms: Option<i64>,
    old_reset: Option<i64>,
    new_reset: Option<i64>,
) -> bool {
    period_ms.is_some_and(|p| p >= LONG_WINDOW_MS)
        || (period_ms.is_none()
            && matches!((old_reset, new_reset), (Some(o), Some(n)) if n - o >= LONG_WINDOW_MS))
}

/// Compact duration for toast text: `6d 23h`, `4h 58m`, `12m`. Negative
/// durations clamp to `0m`.
fn compact_duration(ms: i64) -> String {
    let ms = ms.max(0);
    const MIN: i64 = 60_000;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    if ms >= DAY {
        format!("{}d {}h", ms / DAY, (ms % DAY) / HOUR)
    } else if ms >= HOUR {
        format!("{}h {}m", ms / HOUR, (ms % HOUR) / MIN)
    } else {
        format!("{}m", ms / MIN)
    }
}

pub fn evaluate(snapshots: &[Snapshot], cfg: &Value) -> Vec<Alert> {
    let want = |key: &str| cfg.get(key).and_then(Value::as_bool).unwrap_or(false);
    let want_almost = want("notifyAlmostOut");
    let want_close = want("notifyCuttingClose");
    let want_runout = want("notifyWillRunOut");
    let want_reset = want("notifyReset");
    if !(want_almost || want_close || want_runout || want_reset) {
        return Vec::new();
    }

    let mut alerts = Vec::new();
    let Ok(mut map) = states().lock() else { return alerts };

    for snapshot in snapshots.iter().filter(|s| s.status == "ok") {
        // Restored snapshots past the UI grace window are last-good, not
        // live. Skipping them means stale data can neither fire a new
        // worsening alert nor reset the armed state the live data left.
        // The 3-minute grace (`stale` still false, `attempt_failed` true)
        // still alerts — same as today.
        if snapshot.stale {
            continue;
        }
        if snapshot.id.starts_with("sub2api@") {
            let disabled = cfg.get("disabled").and_then(Value::as_array);
            if disabled.is_some_and(|ids| ids.iter().any(|id| {
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

            // A reset time that jumped forward means the quota window
            // rolled over. The usage guard blocks sliding-window providers
            // whose resets_at creeps forward every refresh while usage
            // stays put: only a near-empty (or sharply dropped) reading
            // counts as a real reset.
            let advanced = matches!(
                (entry.resets_at, metric.resets_at),
                (Some(old), Some(new)) if new - old > 10 * 60_000
            );
            let rolled_over = entry.seen
                && advanced
                && long_window(metric.period_ms, entry.resets_at, metric.resets_at)
                && (used < 2.0 || entry.prev_used.is_some_and(|p| used + 10.0 < p));

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

            if want_reset && rolled_over {
                let shown = crate::i18n::metric_label(cfg, &metric.label);
                let name = format!("{} {}", snapshot.name, shown);
                let loc = crate::i18n::resolved_locale(cfg);
                let next = metric.resets_at.map(|resets| {
                    compact_duration(resets - chrono::Utc::now().timestamp_millis())
                });
                alerts.push(Alert {
                    title: match loc {
                        "zh" => "额度已重置".into(),
                        "ru" => "Лимит сброшен".into(),
                        _ => "Limit reset".into(),
                    },
                    body: match loc {
                        "zh" => format!(
                            "{}{}",
                            if used < 2.0 {
                                format!("{name} 已恢复到 100%。")
                            } else {
                                format!("{name} 已重置 — 可用 {left:.0}%。")
                            },
                            next.map_or(String::new(), |rel| format!(" 下次重置：{rel} 后。"))
                        ),
                        "ru" => format!(
                            "{}{}",
                            if used < 2.0 {
                                format!("{name} снова 100%.")
                            } else {
                                format!("{name} сброшен — доступно {left:.0}%.")
                            },
                            next.map_or(String::new(), |rel| {
                                format!(" Следующий сброс через {rel}.")
                            })
                        ),
                        _ => format!(
                            "{}{}",
                            if used < 2.0 {
                                format!("{name} is back to 100%.")
                            } else {
                                format!("{name} has reset — {left:.0}% available.")
                            },
                            next.map_or(String::new(), |rel| format!(" Next reset in {rel}."))
                        ),
                    },
                });
            }

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
            // After the wipe, so a rollover restarts the usage history.
            entry.prev_used = Some(used);
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
    fn stale_snapshots_of_any_provider_never_fire_or_re_arm_alerts() {
        let id = "codex";
        let cfg = serde_json::json!({"notifyAlmostOut": true, "locale": "en"});
        let make = |used, stale| {
            let mut s = Snapshot::ok(
                id,
                "Codex",
                None,
                vec![crate::providers::Metric::progress("Weekly", used, None)],
            );
            s.stale = stale;
            s
        };
        forget_snapshot(id);
        assert!(evaluate(&[make(50.0, false)], &cfg).is_empty());
        assert!(evaluate(&[make(95.0, true)], &cfg).is_empty());
        assert_eq!(evaluate(&[make(95.0, false)], &cfg).len(), 1);
        assert!(evaluate(&[make(50.0, true)], &cfg).is_empty());
        assert!(evaluate(&[make(95.0, false)], &cfg).is_empty());
        assert!(evaluate(&[make(50.0, false)], &cfg).is_empty());
        assert_eq!(evaluate(&[make(95.0, false)], &cfg).len(), 1);
        forget_snapshot(id);
    }

    #[test]
    fn reset_fires_once_when_period_advances() {
        let id = "codex@reset-advance";
        let cfg = serde_json::json!({"notifyReset": true, "locale": "en"});
        let period = 7 * 86_400_000_i64;
        let t = chrono::Utc::now().timestamp_millis() + 2 * 3_600_000;
        let make = |used, resets| Snapshot::ok(id, "Codex", None, vec![
            crate::providers::Metric::progress("Weekly", used, None)
                .with_reset(Some(resets), Some(period)),
        ]);
        forget_snapshot(id);
        assert!(evaluate(&[make(80.0, t)], &cfg).is_empty());
        assert!(evaluate(&[make(80.0, t)], &cfg).is_empty());
        let alerts = evaluate(&[make(0.0, t + period)], &cfg);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].title, "Limit reset");
        assert!(alerts[0].body.contains("Codex Weekly is back to 100%"));
        assert!(alerts[0].body.contains("Next reset in"));
        assert!(evaluate(&[make(0.0, t + period)], &cfg).is_empty());
        forget_snapshot(id);
    }

    #[test]
    fn reset_after_usage_resumed_reports_remaining() {
        let id = "codex@reset-resumed";
        let cfg = serde_json::json!({"notifyReset": true, "locale": "en"});
        let period = 7 * 86_400_000_i64;
        let t = chrono::Utc::now().timestamp_millis() + 2 * 3_600_000;
        let make = |used, resets| Snapshot::ok(id, "Codex", None, vec![
            crate::providers::Metric::progress("Weekly", used, None)
                .with_reset(Some(resets), Some(period)),
        ]);
        forget_snapshot(id);
        assert!(evaluate(&[make(80.0, t)], &cfg).is_empty());
        // Usage already resumed (20% burned in the new window): report
        // what's left, not a full quota.
        let alerts = evaluate(&[make(20.0, t + period)], &cfg);
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].body.contains("has reset — 80% available"));
        assert!(!alerts[0].body.contains("100%"));
        forget_snapshot(id);
    }

    #[test]
    fn reset_fires_for_untouched_window() {
        let id = "codex@reset-untouched";
        let cfg = serde_json::json!({"notifyReset": true, "locale": "en"});
        let period = 7 * 86_400_000_i64;
        let t = chrono::Utc::now().timestamp_millis() + 3_600_000;
        let make = |used, resets| Snapshot::ok(id, "Codex", None, vec![
            crate::providers::Metric::progress("Weekly", used, None)
                .with_reset(Some(resets), Some(period)),
        ]);
        forget_snapshot(id);
        assert!(evaluate(&[make(0.0, t)], &cfg).is_empty());
        assert_eq!(evaluate(&[make(0.0, t + period)], &cfg).len(), 1);
        forget_snapshot(id);
    }

    #[test]
    fn sliding_window_drift_does_not_fire() {
        let id = "codex@reset-drift";
        let cfg = serde_json::json!({"notifyReset": true, "locale": "en"});
        let t = chrono::Utc::now().timestamp_millis() + 3_600_000;
        let make = |used, resets| Snapshot::ok(id, "Codex", None, vec![
            crate::providers::Metric::progress("Weekly", used, None)
                .with_reset(Some(resets), Some(7 * 86_400_000)),
        ]);
        forget_snapshot(id);
        assert!(evaluate(&[make(60.0, t)], &cfg).is_empty());
        // resets_at crept forward while usage climbed: a sliding window,
        // not a rollover.
        assert!(evaluate(&[make(62.0, t + 15 * 60_000)], &cfg).is_empty());
        forget_snapshot(id);
    }

    #[test]
    fn short_window_reset_is_silent() {
        let id = "codex@reset-short";
        let cfg = serde_json::json!({"notifyReset": true, "locale": "en"});
        let period = 5 * 3_600_000_i64;
        let t = chrono::Utc::now().timestamp_millis() + 3_600_000;
        let make = |used, resets| Snapshot::ok(id, "Codex", None, vec![
            crate::providers::Metric::progress("5 Hours", used, None)
                .with_reset(Some(resets), Some(period)),
        ]);
        forget_snapshot(id);
        assert!(evaluate(&[make(80.0, t)], &cfg).is_empty());
        // A real rollover — but 5-hour windows don't toast.
        assert!(evaluate(&[make(0.0, t + period)], &cfg).is_empty());
        forget_snapshot(id);
    }

    #[test]
    fn long_window_inferred_from_reset_jump_when_period_missing() {
        let id = "codex@reset-noperiod";
        let cfg = serde_json::json!({"notifyReset": true, "locale": "en"});
        let t = chrono::Utc::now().timestamp_millis() + 3_600_000;
        let make = |used, resets| Snapshot::ok(id, "Codex", None, vec![
            crate::providers::Metric::progress("Weekly", used, None)
                .with_reset(Some(resets), None),
        ]);
        forget_snapshot(id);
        assert!(evaluate(&[make(80.0, t)], &cfg).is_empty());
        // No declared period: a 7-day jump between resets IS the window.
        assert_eq!(evaluate(&[make(0.0, t + 7 * 86_400_000)], &cfg).len(), 1);
        forget_snapshot(id);
    }

    #[test]
    fn reset_needs_baseline() {
        let id = "codex@reset-baseline";
        let cfg = serde_json::json!({"notifyReset": true, "locale": "en"});
        let t = chrono::Utc::now().timestamp_millis() + 3_600_000;
        let make = |used, resets| Snapshot::ok(id, "Codex", None, vec![
            crate::providers::Metric::progress("Weekly", used, None)
                .with_reset(Some(resets), Some(7 * 86_400_000)),
        ]);
        forget_snapshot(id);
        // The first reading after launch is a silent baseline.
        assert!(evaluate(&[make(0.0, t)], &cfg).is_empty());
        forget_snapshot(id);
    }

    #[test]
    fn compact_duration_formats() {
        assert_eq!(compact_duration(6 * 86_400_000 + 23 * 3_600_000), "6d 23h");
        assert_eq!(compact_duration(4 * 3_600_000 + 58 * 60_000), "4h 58m");
        assert_eq!(compact_duration(12 * 60_000), "12m");
        assert_eq!(compact_duration(-5_000), "0m");
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
