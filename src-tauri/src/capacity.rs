//! Weekly-quota capacity ledger (#236): what 100% of a provider's weekly
//! window is worth in tokens and API-equivalent dollars.
//!
//! While a weekly window runs, the cycle's tokens/cost come from the
//! spend scan's hourly buckets (spend::window_totals) — a live estimate
//! of what 100% would cost. When Pane actually observes a window at 100%
//! the cycle is frozen: the totals at that moment become an "observed"
//! sample and are never repriced. A window that resets before 100% is
//! recorded "incomplete" — partial coverage (usage on other devices or a
//! shared account) can only under-count, so it never counts as a sample.
//!
//! State lives in %APPDATA%\Pane\quota_cycles.json, written atomically
//! with the same owner-only helper the credential stores use.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::providers;
use crate::providers::onenewapi::store::atomic_write;

/// History depth per provider — enough to average over, small enough to
/// stay readable in the popover and cheap on disk.
const HISTORY_MAX: usize = 12;

/// resets_at can jitter a few minutes between polls of the same window
/// (the API recomputes it); treat starts within this slack as one cycle.
const SAME_WINDOW_MS: i64 = 30 * 60_000;

/// The estimate becomes meaningful once a real fraction of the window is
/// spent — below it, dividing by a sliver of a percent explodes.
const ESTIMATE_MIN_PCT: f64 = 5.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Active,
    Observed,
    Incomplete,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cycle {
    pub start_ms: i64,
    pub end_ms: i64,
    /// Highest used-percent seen this window (≥100 once observed).
    pub peak_pct: f64,
    /// Tokens/cost accumulated in the window. Frozen at the moment a
    /// cycle is observed at 100% — never repriced afterwards.
    pub tokens: f64,
    pub cost: f64,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_ms: Option<i64>,
}

/// Per-provider ledger: the running window plus its finished ones,
/// newest first.
#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<Cycle>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<Cycle>,
}

/// Advance one provider's ledger by one usage poll. `totals` is the
/// spend scan's (cost, tokens) inside the window, or None when no scan
/// has completed yet — an unknown scan result updates the progress peak
/// but never touches the money.
pub fn update(
    entry: &Entry,
    window_start_ms: i64,
    resets_at_ms: i64,
    used_pct: f64,
    totals: Option<(f64, f64)>,
    now_ms: i64,
) -> Entry {
    let mut out = entry.clone();
    let same_cycle = out
        .current
        .as_ref()
        .is_some_and(|c| (window_start_ms - c.start_ms).abs() <= SAME_WINDOW_MS);
    if !same_cycle {
        // The window moved (rollover, or a spent reset credit restarted
        // it): archive the old current as observed or incomplete, then
        // open the new cycle.
        if let Some(mut c) = out.current.take() {
            if c.status != Status::Observed {
                c.status = Status::Incomplete;
            }
            out.history.insert(0, c);
            out.history.truncate(HISTORY_MAX);
        }
        out.current = Some(Cycle {
            start_ms: window_start_ms,
            end_ms: resets_at_ms,
            peak_pct: used_pct,
            tokens: totals.map(|(_, t)| t).unwrap_or(0.0),
            cost: totals.map(|(c, _)| c).unwrap_or(0.0),
            status: Status::Active,
            observed_at_ms: None,
        });
    }
    let Some(c) = out.current.as_mut() else {
        return out;
    };
    // An observed cycle is a sealed sample — later polls and later scans
    // must not reprice it.
    if c.status != Status::Observed {
        c.peak_pct = c.peak_pct.max(used_pct);
        c.end_ms = resets_at_ms;
        if let Some((cost, tokens)) = totals {
            c.cost = cost;
            c.tokens = tokens;
        }
        if used_pct >= 100.0 {
            c.status = Status::Observed;
            c.peak_pct = c.peak_pct.max(100.0);
            c.observed_at_ms = Some(now_ms);
        }
    }
    out
}

/// Mean of the observed samples only — active and incomplete weeks are
/// never evidence of a full quota's worth.
fn observed_average(entry: &Entry) -> Option<(f64, f64, usize)> {
    let mut cost = 0.0;
    let mut tokens = 0.0;
    let mut n = 0usize;
    for c in &entry.history {
        if c.status == Status::Observed {
            cost += c.cost;
            tokens += c.tokens;
            n += 1;
        }
    }
    (n > 0).then(|| (cost / n as f64, tokens / n as f64, n))
}

/// Totals that were never reported by a scan (or genuinely zero) are not
/// a usable basis for an estimate.
fn totals_known(c: &Cycle) -> bool {
    c.cost > 0.0 || c.tokens > 0.0
}

/// Estimated full-window (cost, tokens) for the active cycle, once the
/// spent fraction is large enough to divide by.
fn estimate(c: &Cycle) -> Option<(f64, f64)> {
    if c.status != Status::Active || c.peak_pct < ESTIMATE_MIN_PCT || !totals_known(c) {
        return None;
    }
    let frac = c.peak_pct / 100.0;
    Some((c.cost / frac, c.tokens / frac))
}

fn fmt_usd(v: f64) -> String {
    if v >= 1000.0 {
        format!("${:.1}K", v / 1000.0)
    } else {
        format!("${:.2}", v)
    }
}

fn fmt_usd_est(v: f64) -> String {
    if v >= 1000.0 {
        format!("${:.1}K", v / 1000.0)
    } else if v >= 100.0 {
        format!("${:.0}", v)
    } else {
        format!("${:.2}", v)
    }
}

fn fmt_tokens(v: f64) -> String {
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

fn cycle_json(c: &Cycle) -> Value {
    json!({
        "start_ms": c.start_ms,
        "end_ms": c.end_ms,
        "peak_pct": c.peak_pct,
        "tokens": c.tokens,
        "cost": c.cost,
        "status": c.status,
        "observed_at_ms": c.observed_at_ms,
    })
}

/// The card row: `value` is the plain-text fallback the HTTP API serves;
/// `detail` carries the structured ledger the frontend popover renders.
pub fn metric(entry: &Entry) -> Option<providers::Metric> {
    let cur = entry.current.as_ref()?;
    let est = estimate(cur);
    let value = if cur.status == Status::Observed {
        format!("{} · {} tokens", fmt_usd(cur.cost), fmt_tokens(cur.tokens))
    } else if let Some((est_cost, est_tokens)) = est {
        format!("≈ {} · {} tokens", fmt_usd_est(est_cost), fmt_tokens(est_tokens))
    } else if totals_known(cur) {
        format!("{} · {} so far", fmt_usd(cur.cost), fmt_tokens(cur.tokens))
    } else {
        // No spend scan has landed yet — say so rather than print zeros.
        "collecting…".to_string()
    };

    let mut current = cycle_json(cur);
    current["used_pct"] = json!(cur.peak_pct);
    if let Some((est_cost, est_tokens)) = est {
        current["est_cost"] = json!(est_cost);
        current["est_tokens"] = json!(est_tokens);
    }
    let mut detail = json!({
        "current": current,
        "history": entry.history.iter().map(cycle_json).collect::<Vec<_>>(),
    });
    if let Some((cost, tokens, n)) = observed_average(entry) {
        detail["avg"] = json!({ "cost": cost, "tokens": tokens, "n": n });
    }

    let mut m = providers::Metric::text("Weekly capacity", value);
    m.detail = serde_json::to_string(&detail).ok();
    Some(m)
}

fn path() -> std::path::PathBuf {
    providers::config_dir().join("quota_cycles.json")
}

/// Lazily-loaded ledger map, shared by every fetch_usage pass.
fn ledger() -> &'static Mutex<Option<HashMap<String, Entry>>> {
    static LEDGER: OnceLock<Mutex<Option<HashMap<String, Entry>>>> = OnceLock::new();
    LEDGER.get_or_init(|| Mutex::new(None))
}

fn load(map: &mut Option<HashMap<String, Entry>>) {
    if map.is_some() {
        return;
    }
    let loaded = std::fs::read_to_string(path())
        .ok()
        .and_then(|raw| serde_json::from_str::<HashMap<String, Entry>>(&raw).ok())
        .unwrap_or_default();
    *map = Some(loaded);
}

/// One usage poll for a provider's weekly window: advance the ledger and
/// persist it (only when it actually changed — the common steady-state
/// poll writes nothing).
pub fn note_weekly_window(
    id: &str,
    window_start_ms: i64,
    resets_at_ms: i64,
    used_pct: f64,
    totals: Option<(f64, f64)>,
    now_ms: i64,
) -> Entry {
    let mut guard = ledger().lock().unwrap_or_else(|e| e.into_inner());
    load(&mut guard);
    let map = guard.as_mut().expect("load fills the map");
    let prev = map.get(id).cloned().unwrap_or_default();
    let next = update(&prev, window_start_ms, resets_at_ms, used_pct, totals, now_ms);
    if next != prev {
        map.insert(id.to_string(), next.clone());
        if let Ok(raw) = serde_json::to_string_pretty(map) {
            let _ = atomic_write(&path(), &raw);
        }
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    const WEEK: i64 = 7 * 86_400_000;
    const T0: i64 = 1_800_000_000_000; // fixed epoch ms

    fn poll(entry: &Entry, start: i64, pct: f64, totals: Option<(f64, f64)>) -> Entry {
        update(entry, start, start + WEEK, pct, totals, T0 + 1_000)
    }

    #[test]
    fn estimate_only_once_five_percent_spent() {
        let e = poll(&Entry::default(), T0, 4.9, Some((10.0, 80_000_000.0)));
        let cur = e.current.as_ref().unwrap();
        assert_eq!(cur.status, Status::Active);
        assert!(estimate(cur).is_none(), "4.9% is too thin to divide by");
        let m = metric(&e).unwrap();
        assert!(m.value.as_deref().unwrap().contains("so far"));

        let e = poll(&e, T0, 5.0, Some((10.0, 80_000_000.0)));
        let cur = e.current.as_ref().unwrap();
        let (cost, tokens) = estimate(cur).expect("5% estimates");
        assert!((cost - 200.0).abs() < 1e-9);
        assert!((tokens - 1_600_000_000.0).abs() < 1e-3);
        let m = metric(&e).unwrap();
        assert!(m.value.as_deref().unwrap().starts_with('≈'));
        let detail: Value = serde_json::from_str(m.detail.as_deref().unwrap()).unwrap();
        assert!(detail["current"]["est_cost"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn observing_100_freezes_the_sample() {
        let e = poll(&Entry::default(), T0, 40.0, Some((40.0, 400_000_000.0)));
        let e = poll(&e, T0, 100.0, Some((218.0, 1_300_000_000.0)));
        let cur = e.current.as_ref().unwrap();
        assert_eq!(cur.status, Status::Observed);
        assert_eq!(cur.cost, 218.0);
        assert_eq!(cur.tokens, 1_300_000_000.0);
        assert!(cur.observed_at_ms.is_some());
        assert!(cur.peak_pct >= 100.0);

        // Later polls and fresher scans never reprice a sealed sample.
        let e2 = poll(&e, T0, 100.0, Some((300.0, 9_000_000_000.0)));
        let cur2 = e2.current.as_ref().unwrap();
        assert_eq!(cur2.cost, 218.0);
        assert_eq!(cur2.tokens, 1_300_000_000.0);
        // An observed row drops the ≈ — the numbers are real.
        let m = metric(&e2).unwrap();
        assert!(!m.value.as_deref().unwrap().starts_with('≈'));
        let detail: Value = serde_json::from_str(m.detail.as_deref().unwrap()).unwrap();
        assert!(detail["current"]["est_cost"].is_null());
    }

    #[test]
    fn rollover_archives_unobserved_as_incomplete() {
        let e = poll(&Entry::default(), T0, 82.0, Some((150.0, 900_000_000.0)));
        let e = poll(&e, T0 + WEEK, 3.0, Some((1.0, 10_000_000.0)));
        assert_eq!(e.history.len(), 1);
        assert_eq!(e.history[0].status, Status::Incomplete);
        assert_eq!(e.history[0].peak_pct, 82.0);
        assert_eq!(e.history[0].cost, 150.0);
        assert_eq!(e.current.as_ref().unwrap().status, Status::Active);
    }

    #[test]
    fn rollover_after_observed_keeps_the_sample() {
        let e = poll(&Entry::default(), T0, 100.0, Some((218.0, 1_300_000_000.0)));
        let e = poll(&e, T0 + WEEK, 2.0, Some((0.5, 5_000_000.0)));
        assert_eq!(e.history[0].status, Status::Observed);
        assert_eq!(e.history[0].cost, 218.0);
    }

    #[test]
    fn reset_jitter_within_thirty_minutes_is_the_same_cycle() {
        let e = poll(&Entry::default(), T0, 40.0, Some((40.0, 400_000_000.0)));
        let e = update(&e, T0 + 20 * 60_000, T0 + WEEK + 20 * 60_000, 41.0, Some((41.0, 410_000_000.0)), T0 + 2_000);
        assert!(e.history.is_empty(), "jitter must not archive the cycle");
        assert_eq!(e.current.as_ref().unwrap().peak_pct, 41.0);
        // Beyond the slack it's a genuinely different window.
        let e = update(&e, T0 + WEEK, T0 + 2 * WEEK, 1.0, None, T0 + 3_000);
        assert_eq!(e.history.len(), 1);
    }

    #[test]
    fn history_is_newest_first_and_capped_at_twelve() {
        let mut e = Entry::default();
        for w in 0..15i64 {
            e = update(&e, T0 + w * WEEK, T0 + (w + 1) * WEEK, 10.0, Some((w as f64, 0.0)), T0);
        }
        assert_eq!(e.history.len(), 12);
        // The 15th poll is the active current; history holds w13..w2.
        assert_eq!(e.history[0].start_ms, T0 + 13 * WEEK, "newest first");
        assert_eq!(e.history[11].start_ms, T0 + 2 * WEEK);
    }

    #[test]
    fn average_counts_observed_weeks_only() {
        let mut e = Entry::default();
        // Week 1 observed at $200/1B, week 2 incomplete at $150, week 3
        // observed at $260/1.6B, week 4 (current) active.
        e = update(&e, T0, T0 + WEEK, 100.0, Some((200.0, 1_000_000_000.0)), T0);
        e = update(&e, T0 + WEEK, T0 + 2 * WEEK, 60.0, Some((150.0, 0.0)), T0);
        e = update(&e, T0 + 2 * WEEK, T0 + 3 * WEEK, 100.0, Some((260.0, 1_600_000_000.0)), T0);
        e = update(&e, T0 + 3 * WEEK, T0 + 4 * WEEK, 10.0, Some((20.0, 0.0)), T0);
        let (cost, tokens, n) = observed_average(&e).unwrap();
        assert_eq!(n, 2);
        assert!((cost - 230.0).abs() < 1e-9);
        assert!((tokens - 1_300_000_000.0).abs() < 1e-3);
        let m = metric(&e).unwrap();
        let detail: Value = serde_json::from_str(m.detail.as_deref().unwrap()).unwrap();
        assert_eq!(detail["avg"]["n"], 2);
    }

    #[test]
    fn none_totals_updates_the_peak_but_not_the_money() {
        let e = poll(&Entry::default(), T0, 40.0, Some((40.0, 400_000_000.0)));
        // A poll ahead of the first spend scan: pct moves, dollars don't.
        let e = update(&e, T0, T0 + WEEK, 55.0, None, T0 + 5_000);
        let cur = e.current.as_ref().unwrap();
        assert_eq!(cur.peak_pct, 55.0);
        assert_eq!(cur.cost, 40.0);
        assert_eq!(cur.tokens, 400_000_000.0);
    }
}
