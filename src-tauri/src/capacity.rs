//! Weekly-quota capacity ledger (#236): what 100% of a provider's weekly
//! window is worth in tokens and API-equivalent dollars.
//!
//! While a weekly window runs, the cycle's tokens/cost come from the
//! spend scan's hourly buckets (spend::window_totals) — a live estimate
//! of what 100% would cost. When Pane observes a window at 100% the
//! cycle is frozen once a scan STARTED at-or-after that moment lands
//! (the scan runs concurrently, so files read before the hit can't
//! prove the final usage); those totals become an "observed" sample,
//! never repriced.
//! A window that resets before 100% is
//! recorded "incomplete" — partial coverage (usage on other devices or a
//! shared account) can only under-count, so it never counts as a sample.
//!
//! State lives in %APPDATA%\Pane\quota_cycles.json, written atomically
//! with the same owner-only helper the credential stores use.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::providers;
use crate::providers::onenewapi::store::atomic_write;
use crate::spend::{self, WindowTotals};

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
    /// When a poll first saw this window at ≥100%. Set while the cycle
    /// stays Active: the spend scan runs concurrently with the usage
    /// fetch, so totals from a scan STARTED before this moment can't
    /// prove the final usage — sealing waits for a newer scan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_full_at_ms: Option<i64>,
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

/// One stretch of time a default card's shared log dir belonged to one
/// account. `to_ms` is None while the account is still signed in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Interval {
    pub tag: String,
    pub from_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_ms: Option<i64>,
}

/// Ownership history of a default card's log dir — a single account
/// can hold several disjoint stretches (A → B → A), and each must
/// count only the logs it wrote.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct IdentityRecord {
    pub intervals: Vec<Interval>,
}

impl<'de> serde::Deserialize<'de> for IdentityRecord {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Intervals { intervals: Vec<Interval> },
            // The round-5 shape: one switch timestamp per account.
            Stamp { tag: String, since_ms: i64 },
        }
        Ok(match Repr::deserialize(d)? {
            Repr::Intervals { intervals } => IdentityRecord { intervals },
            Repr::Stamp { tag, since_ms } => IdentityRecord {
                intervals: vec![Interval { tag, from_ms: since_ms, to_ms: None }],
            },
        })
    }
}

/// On-disk shape of quota_cycles.json: the cycle ledgers plus the
/// default-card identity stamps. Older files were the bare cycles map
/// — load() migrates them on first read.
#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
struct LedgerFile {
    #[serde(default)]
    cycles: HashMap<String, Entry>,
    #[serde(default)]
    identities: HashMap<String, IdentityRecord>,
}

/// Advance one provider's ledger by one usage poll. `totals` is the
/// spend scan's bounded WindowTotals inside the window, or None when no
/// scan has completed yet — an unknown scan result updates the progress
/// peak but never touches the money.
pub fn update(
    entry: &Entry,
    window_start_ms: i64,
    resets_at_ms: i64,
    used_pct: f64,
    totals: Option<WindowTotals>,
    now_ms: i64,
) -> Entry {
    let mut out = entry.clone();
    // Same window when the start matches within jitter — EXCEPT a reset
    // credit spent mid-week restarts the window a few minutes forward
    // with usage back near zero: a forward shift plus a material drop,
    // which jitter can't produce (usage is monotonic inside a window).
    // A backward shift with a drop stays the same cycle — providers can
    // recompute starts backwards, and over-triggering would split one
    // real week into two fragments.
    let same_cycle = out.current.as_ref().is_some_and(|c| {
        (window_start_ms - c.start_ms).abs() <= SAME_WINDOW_MS
            && !(window_start_ms > c.start_ms + 60_000 && used_pct + 2.0 < c.peak_pct)
    });
    if !same_cycle {
        // The window moved (rollover, or a spent reset credit restarted
        // it): archive the old current as observed or incomplete, then
        // open the new cycle.
        if let Some(mut c) = out.current.take() {
            if c.status != Status::Observed {
                // A cycle Pane saw reach 100% but never sealed keeps
                // hit_full_at_ms and no observed_at_ms — "finalizing":
                // it reached 100%, but a scan started after the hit may
                // still confirm the money (resolve_pending retries it).
                c.status = if c.hit_full_at_ms.is_some() {
                    Status::Observed
                } else {
                    Status::Incomplete
                };
            }
            out.history.insert(0, c);
            out.history.truncate(HISTORY_MAX);
        }
        out.current = Some(Cycle {
            start_ms: window_start_ms,
            end_ms: resets_at_ms,
            peak_pct: used_pct,
            tokens: totals.map(|t| t.tokens).unwrap_or(0.0),
            cost: totals.map(|t| t.cost).unwrap_or(0.0),
            status: Status::Active,
            observed_at_ms: None,
            hit_full_at_ms: None,
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
        if let Some(t) = totals {
            c.cost = t.cost;
            c.tokens = t.tokens;
        }
        if used_pct >= 100.0 && c.hit_full_at_ms.is_none() {
            c.hit_full_at_ms = Some(now_ms);
        }
        // Sealing needs totals from a COMPLETE scan STARTED at-or-after
        // the moment 100% was first seen — possibly this same poll if
        // the scan is already newer — while the window's hours are
        // still reconstructible (always true for a ≤7-day window; the
        // same rule resolve_pending applies to archived cycles). A
        // scan with a swallowed read gap, or files read before the
        // hit, can't prove the final usage.
        if let (Some(hit), Some(t)) = (c.hit_full_at_ms, totals) {
            if t.complete && t.scan_started_ms >= hit && reconstructible(c, now_ms) {
                c.status = Status::Observed;
                c.peak_pct = c.peak_pct.max(100.0);
                c.observed_at_ms = Some(now_ms);
            }
        }
    }
    out
}

/// An observed cycle with no observed_at_ms is "finalizing": it
/// reached 100% and rolled over, but no scan started after the hit has
/// confirmed its totals yet.
fn is_pending(c: &Cycle) -> bool {
    c.status == Status::Observed && c.observed_at_ms.is_none()
}

/// A cycle's totals are only recomputable while its start hour is still
/// inside the spend scan's retention window (spend::HOURS_WINDOW_MS;
/// +1 h slack for the floored start bucket). Past that, a pending cycle
/// can never be confirmed and must give up.
fn reconstructible(c: &Cycle, now_ms: i64) -> bool {
    now_ms - spend::HOURS_WINDOW_MS + 3_600_000 <= c.start_ms
}

/// Retry sealing a pending (rolled-over unsealed) cycle against a
/// newer scan. `totals` must be window_totals bounded to the cycle's
/// own [start, end] hours — any scan refines the displayed numbers,
/// but sealing needs a COMPLETE scan STARTED at-or-after hit_full_at_ms.
/// Once the start hour ages out of retention the money can never be
/// confirmed: give up as incomplete with its last totals.
pub fn resolve_pending(c: &Cycle, totals: Option<WindowTotals>, now_ms: i64) -> Cycle {
    let mut out = c.clone();
    if !is_pending(&out) {
        return out;
    }
    // Fresher scans improve the displayed totals even when they can't
    // confirm them.
    if let Some(t) = totals {
        out.cost = t.cost;
        out.tokens = t.tokens;
    }
    if !reconstructible(&out, now_ms) {
        out.status = Status::Incomplete;
        return out;
    }
    if let (Some(hit), Some(t)) = (out.hit_full_at_ms, totals) {
        if t.complete && t.scan_started_ms >= hit {
            out.observed_at_ms = Some(now_ms);
        }
    }
    out
}

/// Mean of the sealed observed samples only — active, incomplete, and
/// still-finalizing weeks are never evidence of a full quota's worth.
/// The running cycle counts once it's observed; it needn't wait for
/// rollover into history.
fn observed_average(entry: &Entry) -> Option<(f64, f64, usize)> {
    let mut cost = 0.0;
    let mut tokens = 0.0;
    let mut n = 0usize;
    for c in entry.current.iter().chain(entry.history.iter()) {
        if c.status == Status::Observed && c.observed_at_ms.is_some() {
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
    // At or past 100% (a pending seal, or overage) the estimate IS the
    // observed totals — dividing by >100% would shrink below reality.
    if c.peak_pct >= 100.0 {
        return Some((c.cost, c.tokens));
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

/// The family's default-account identity — the same value the
/// snapshot-cache stamp uses to detect an account swap
/// (providers::{claude,codex}::default_identity).
fn default_identity_for(family: &str) -> Option<String> {
    match family {
        "claude" => providers::claude::default_identity(),
        "codex" => providers::codex::default_identity(),
        _ => None,
    }
}

/// The 8-char account tag for a bare default id — the same first-8
/// non-dash truncation the `claude@<hash8>` scoped ids use, and never
/// a raw email. Scoped cards already embed their account → None.
fn identity_tag_of(id: &str, identity: Option<&str>) -> Option<String> {
    if id.contains('@') {
        return None;
    }
    let tag: String = identity?.chars().filter(|c| *c != '-').take(8).collect();
    (!tag.is_empty()).then_some(tag)
}

/// This snapshot's account tag when it is a default card with a known
/// identity — the stamp quota_cycles.json keys its switch tracking by.
pub fn identity_tag(id: &str, family: &str) -> Option<String> {
    identity_tag_of(id, default_identity_for(family).as_deref())
}

/// Ledger key for a snapshot. A default card keeps the bare `claude` /
/// `codex` id across a sign-in change, so two different accounts would
/// otherwise share one cycle history — key it by `{id}|{identity8}`.
/// An unreadable identity falls back to the bare id: a transient
/// failure mustn't orphan the ledger mid-session. Switching back to an
/// earlier account restores ITS ledger naturally.
fn ledger_key_from(id: &str, identity: Option<&str>) -> String {
    match identity_tag_of(id, identity) {
        Some(tag) => format!("{id}|{tag}"),
        None => id.to_string(),
    }
}

/// The pre-fetch tag for THIS snapshot: only a bare default id owns
/// the shared dir a tag was captured for — a scoped `claude@<hash8>`
/// card must not inherit the default account's tag (its post-fetch
/// tag is always None, so that pairing would read as a swap forever).
pub fn start_tag_for<'a>(
    id: &str,
    claude: Option<&'a str>,
    codex: Option<&'a str>,
) -> Option<&'a str> {
    match id {
        "claude" => claude,
        "codex" => codex,
        _ => None,
    }
}

/// A scoped `claude@<hash8>` / `codex@<hash8>` id was minted from the
/// account found at discovery time — it IS the pre-fetch identity.
/// Re-discovered after the fetch, the same id means the dir still
/// holds that account; a missing id means the dir re-signed-in and a
/// different account now owns it, so the in-flight poll can't be
/// attributed. Bare ids don't participate — the pre/post tag rule
/// covers them.
pub fn scoped_identity_stable(id: &str, fresh_ids: &HashSet<String>) -> bool {
    !id.contains('@') || fresh_ids.contains(id)
}

/// The key for an ALREADY-resolved tag — when the tag was captured
/// earlier in the refresh (the pre-fetch read wins over a mid-refresh
/// sign-in swap).
pub fn ledger_key_for(id: &str, tag: Option<&str>) -> String {
    match tag {
        Some(t) if !id.contains('@') => format!("{id}|{t}"),
        _ => id.to_string(),
    }
}

/// Lazily-loaded ledger file, shared by every fetch_usage pass.
fn ledger() -> &'static Mutex<Option<LedgerFile>> {
    static LEDGER: OnceLock<Mutex<Option<LedgerFile>>> = OnceLock::new();
    LEDGER.get_or_init(|| Mutex::new(None))
}

fn load(map: &mut Option<LedgerFile>) {
    if map.is_some() {
        return;
    }
    let loaded = std::fs::read_to_string(path())
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .map(|doc| {
            // The new shape carries `cycles`/`identities`; a file that
            // is the bare provider→Entry map is the old shape — read it
            // as cycles with no identity stamps.
            if doc.get("cycles").is_some() || doc.get("identities").is_some() {
                serde_json::from_value::<LedgerFile>(doc).unwrap_or_default()
            } else {
                LedgerFile {
                    cycles: serde_json::from_value(doc).unwrap_or_default(),
                    identities: HashMap::new(),
                }
            }
        })
        .unwrap_or_default();
    *map = Some(loaded);
}

/// Cap on stored ownership intervals per default card — a switcher
/// can't grow quota_cycles.json without bound.
const IDENTITY_INTERVALS_MAX: usize = 32;

/// Fold this poll's account tag into the ownership history. First
/// sighting opens `[0, ∞)` — Pane can't know about earlier switches,
/// so nothing is excluded. A different tag, including a switch BACK
/// to a previous account, closes the open interval at now and starts
/// a fresh one — the shared dir holds the other account's logs in
/// between. Closed intervals older than the spend scan's retention
/// can't bound any window anymore and are dropped.
fn observe_identity(intervals: &[Interval], tag: &str, now_ms: i64) -> Vec<Interval> {
    let mut out = intervals.to_vec();
    match out.last_mut() {
        Some(open) if open.to_ms.is_none() && open.tag == tag => {}
        Some(open) if open.to_ms.is_none() => {
            open.to_ms = Some(now_ms);
            out.push(Interval { tag: tag.into(), from_ms: now_ms, to_ms: None });
        }
        _ => out.push(Interval {
            tag: tag.into(),
            from_ms: if out.is_empty() { 0 } else { now_ms },
            to_ms: None,
        }),
    }
    out.retain(|i| i.to_ms.map_or(true, |to| to >= now_ms - spend::HOURS_WINDOW_MS));
    if out.len() > IDENTITY_INTERVALS_MAX {
        out.drain(..out.len() - IDENTITY_INTERVALS_MAX);
    }
    out
}

/// The stretches of `[win_start, win_end)` this account owned the
/// shared dir — several when it signed out and back inside the window.
fn owned_ranges(
    intervals: &[Interval],
    tag: &str,
    win_start: i64,
    win_end: i64,
) -> Vec<(i64, i64)> {
    intervals
        .iter()
        .filter(|i| i.tag == tag)
        .map(|i| (i.from_ms.max(win_start), i.to_ms.unwrap_or(i64::MAX).min(win_end)))
        .filter(|(a, b)| a < b)
        .collect()
}

/// Where this poll may read spend inside the window: just the ranges
/// the current account owned. A scoped id or unreadable identity owns
/// the whole window (the card's logs are already per-account).
fn ranges_for(
    identity_tag: Option<&str>,
    intervals: &[Interval],
    win_start: i64,
    win_end: i64,
) -> Vec<(i64, i64)> {
    match identity_tag {
        Some(tag) => owned_ranges(intervals, tag, win_start, win_end),
        None => vec![(win_start, win_end)],
    }
}

/// Sum a window's hourly totals over the ranges this account owned.
/// Adjacent owners both count the hour holding the switch moment —
/// buckets are whole hours, so the boundary hour lands in both
/// ledgers' ranges.
fn owned_window_totals(
    spend_id: &str,
    identity_tag: Option<&str>,
    intervals: &[Interval],
    win_start: i64,
    win_end: i64,
) -> Option<WindowTotals> {
    let ranges = ranges_for(identity_tag, intervals, win_start, win_end);
    if ranges.is_empty() {
        // This account owned none of the window — a zero total, but
        // still report the scan's freshness so a 100% can seal.
        return spend::window_totals(spend_id, win_start, win_end)
            .map(|t| WindowTotals { cost: 0.0, tokens: 0.0, ..t });
    }
    let mut parts = Vec::with_capacity(ranges.len());
    for (a, b) in ranges {
        parts.push(spend::window_totals(spend_id, a, b)?);
    }
    Some(combine_window_totals(parts))
}

/// Fold per-range totals into one: the scan fields all come from the
/// same publish, but take the conservative min/AND anyway.
fn combine_window_totals(parts: Vec<WindowTotals>) -> WindowTotals {
    let mut out = WindowTotals {
        cost: 0.0,
        tokens: 0.0,
        scan_started_ms: i64::MAX,
        complete: true,
    };
    for t in parts {
        out.cost += t.cost;
        out.tokens += t.tokens;
        out.scan_started_ms = out.scan_started_ms.min(t.scan_started_ms);
        out.complete &= t.complete;
    }
    out
}

/// A stored ledger entry, read without mutating — for restored/stale
/// snapshots, which may carry a stale Weekly % and must never advance
/// the ledger.
pub fn peek(key: &str) -> Option<Entry> {
    let mut guard = ledger().lock().unwrap_or_else(|e| e.into_inner());
    load(&mut guard);
    guard.as_ref()?.cycles.get(key).cloned()
}

/// Only a live poll may advance the ledger: a restored snapshot's
/// numbers are a replay of an older poll, and a failed attempt's
/// restored numbers are older still — both are stale (`stale` /
/// `attempt_failed` as restore_last_success_after_error sets them).
/// `identity_stable` is false when the default account swapped while
/// the provider request was in flight — the poll can't be attributed
/// to a known account at all.
pub fn may_advance(stale: bool, attempt_failed: bool, identity_stable: bool) -> bool {
    !(stale || attempt_failed || !identity_stable)
}

/// Set when an in-memory change (or a failed write) hasn't reached
/// disk. An observed sample exists nowhere else — a dropped write would
/// lose it permanently, so every call retries while the flag is up,
/// even when the new state equals the old.
static PERSIST_DIRTY: AtomicBool = AtomicBool::new(false);

/// A write is owed when this poll changed the ledger, or an earlier
/// write never landed.
fn persist_needed(changed: bool, dirty: bool) -> bool {
    changed || dirty
}

/// One usage poll for a provider's weekly window: advance the ledger
/// and persist it (only when it actually changed or an earlier write
/// failed — the common steady-state poll writes nothing). `spend_id`
/// addresses the card's logs in the spend scan; `identity_tag` is the
/// default card's account tag when known, so a sign-in switch bounds
/// the scan to logs this account wrote.
pub fn note_weekly_window(
    key: &str,
    spend_id: &str,
    identity_tag: Option<&str>,
    window_start_ms: i64,
    resets_at_ms: i64,
    used_pct: f64,
    now_ms: i64,
) -> Entry {
    let mut guard = ledger().lock().unwrap_or_else(|e| e.into_inner());
    load(&mut guard);
    let map = guard.as_mut().expect("load fills the map");
    // A default dir's logs outlive sign-ins — every scan is bounded to
    // the stretches THIS account owned the dir, or a previous login's
    // spend would count toward this account's window.
    let mut stamp_changed = false;
    let intervals = match identity_tag {
        Some(tag) => {
            let empty = IdentityRecord { intervals: Vec::new() };
            let stored = map.identities.get(spend_id).unwrap_or(&empty);
            let next = observe_identity(&stored.intervals, tag, now_ms);
            if next != stored.intervals {
                map.identities.insert(
                    spend_id.to_string(),
                    IdentityRecord { intervals: next.clone() },
                );
                stamp_changed = true;
            }
            next
        }
        None => Vec::new(),
    };
    let prev = map.cycles.get(key).cloned().unwrap_or_default();
    let totals = owned_window_totals(
        spend_id,
        identity_tag,
        &intervals,
        window_start_ms,
        resets_at_ms,
    );
    let mut next = update(&prev, window_start_ms, resets_at_ms, used_pct, totals, now_ms);
    // Pending history cycles (rolled over before their confirming
    // scan) retry against the newest scan, bounded to their own
    // window's hours — a fresh poll is also their next chance to seal.
    for c in next.history.iter_mut() {
        *c = resolve_pending(
            c,
            owned_window_totals(spend_id, identity_tag, &intervals, c.start_ms, c.end_ms),
            now_ms,
        );
    }
    let cycles_changed = next != prev;
    if cycles_changed {
        map.cycles.insert(key.to_string(), next.clone());
    }
    let changed = cycles_changed || stamp_changed;
    if persist_needed(changed, PERSIST_DIRTY.load(Ordering::Relaxed)) {
        match serde_json::to_string_pretty(map) {
            Ok(raw) => match atomic_write(&path(), &raw) {
                Ok(()) => PERSIST_DIRTY.store(false, Ordering::Relaxed),
                Err(e) => {
                    PERSIST_DIRTY.store(true, Ordering::Relaxed);
                    eprintln!("[pane] capacity: persist failed: {e}");
                }
            },
            Err(e) => {
                PERSIST_DIRTY.store(true, Ordering::Relaxed);
                eprintln!("[pane] capacity: serialize failed: {e}");
            }
        }
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    const WEEK: i64 = 7 * 86_400_000;
    const T0: i64 = 1_800_000_000_000; // fixed epoch ms

    /// Totals as a complete scan reports them.
    fn wt(cost: f64, tokens: f64, started_ms: i64) -> WindowTotals {
        WindowTotals { cost, tokens, scan_started_ms: started_ms, complete: true }
    }

    /// A poll at T0+1s whose totals come from a complete scan STARTED
    /// at T0 — always an "older" scan, so a 100% seen here stays pending.
    fn poll(entry: &Entry, start: i64, pct: f64, totals: Option<(f64, f64)>) -> Entry {
        update(
            entry,
            start,
            start + WEEK,
            pct,
            totals.map(|(c, t)| wt(c, t, T0)),
            T0 + 1_000,
        )
    }

    fn iv(tag: &str, from_ms: i64, to_ms: Option<i64>) -> Interval {
        Interval { tag: tag.into(), from_ms, to_ms }
    }

    #[test]
    fn observe_identity_tracks_ownership_switches() {
        // First sighting owns everything — Pane can't know about
        // earlier switches.
        let v = observe_identity(&[], "aaaa1111", T0);
        assert_eq!(v, vec![iv("aaaa1111", 0, None)]);
        // Same tag keeps the open interval untouched.
        assert_eq!(observe_identity(&v, "aaaa1111", T0 + 100), v);
        // A different tag closes the open interval and starts a fresh
        // one — the shared dir holds the other account's logs between.
        let v = observe_identity(&v, "bbbb2222", T0 + 500);
        assert_eq!(
            v,
            vec![iv("aaaa1111", 0, Some(T0 + 500)), iv("bbbb2222", T0 + 500, None)]
        );
        // Switching BACK to the first tag opens a SECOND interval for
        // it — the earlier stretch is preserved, not resurrected.
        let v = observe_identity(&v, "aaaa1111", T0 + 900);
        assert_eq!(
            v,
            vec![
                iv("aaaa1111", 0, Some(T0 + 500)),
                iv("bbbb2222", T0 + 500, Some(T0 + 900)),
                iv("aaaa1111", T0 + 900, None),
            ]
        );
        // Closed intervals beyond the spend scan's retention can't
        // bound any window anymore — dropped; the open one stays. The
        // cutoff lands between the two closed ends (500 and 900).
        let pruned = observe_identity(&v, "aaaa1111", T0 + spend::HOURS_WINDOW_MS + 700);
        assert_eq!(
            pruned,
            vec![
                iv("bbbb2222", T0 + 500, Some(T0 + 900)),
                iv("aaaa1111", T0 + 900, None),
            ],
            "the first A stretch closed at +500 is older than retention"
        );
        // The cap drops the oldest intervals first.
        let mut many = Vec::new();
        for i in 0..IDENTITY_INTERVALS_MAX + 3 {
            many = observe_identity(&many, &format!("t{i:02}"), T0 + i as i64);
        }
        assert_eq!(many.len(), IDENTITY_INTERVALS_MAX);
        assert_eq!(many.last().unwrap().tag, format!("t{:02}", IDENTITY_INTERVALS_MAX + 2));
    }

    #[test]
    fn owned_ranges_clip_the_window_to_ownership() {
        // A owns [0, 500) and [900, ∞), B owns [500, 900). A week
        // spanning all three gives the current account TWO ranges —
        // A's first stretch counts toward A's ledger, B's toward B's.
        let owned = vec![
            iv("aaaa1111", 0, Some(500)),
            iv("bbbb2222", 500, Some(900)),
            iv("aaaa1111", 900, None),
        ];
        assert_eq!(
            owned_ranges(&owned, "aaaa1111", 0, 2000),
            vec![(0, 500), (900, 2000)]
        );
        // A window fully inside B's ownership gives A nothing — its
        // cycle totals are zero, not B's spend.
        assert_eq!(owned_ranges(&owned, "aaaa1111", 600, 800), Vec::<(i64, i64)>::new());
        // Ranges clip to the window's ends.
        assert_eq!(
            owned_ranges(&owned, "bbbb2222", 400, 700),
            vec![(500, 700)]
        );
        // No tag (scoped card or unreadable identity) owns the whole
        // window — the card's logs are already per-account.
        assert_eq!(ranges_for(None, &owned, 100, 600), vec![(100, 600)]);
        assert_eq!(
            ranges_for(Some("aaaa1111"), &owned, 100, 600),
            vec![(100, 500)]
        );
    }

    #[test]
    fn identity_record_migrates_the_single_stamp_shape() {
        // Round-5 files stored one {tag, since_ms} per card — it loads
        // as one open interval; the intervals shape round-trips.
        let rec: IdentityRecord =
            serde_json::from_str(r#"{"tag":"aaaa1111","since_ms":42}"#).unwrap();
        assert_eq!(rec.intervals, vec![iv("aaaa1111", 42, None)]);
        let rec: IdentityRecord = serde_json::from_str(
            r#"{"intervals":[{"tag":"bbbb2222","from_ms":1,"to_ms":2}]}"#,
        )
        .unwrap();
        assert_eq!(rec.intervals, vec![iv("bbbb2222", 1, Some(2))]);
    }

    #[test]
    fn combine_window_totals_sums_and_keeps_the_worst_scan_flag() {
        let out = combine_window_totals(vec![
            wt(10.0, 100.0, T0 + 5),
            wt(4.5, 50.0, T0 + 2),
        ]);
        assert_eq!(out.cost, 14.5);
        assert_eq!(out.tokens, 150.0);
        assert_eq!(out.scan_started_ms, T0 + 2, "earliest scan start wins");
        assert!(out.complete);
        let gapped = combine_window_totals(vec![
            wt(1.0, 1.0, T0),
            WindowTotals { cost: 2.0, tokens: 2.0, scan_started_ms: T0 + 1, complete: false },
        ]);
        assert!(!gapped.complete, "one gapped range unseals the whole window");
        assert_eq!(gapped.scan_started_ms, T0);
    }

    /// The pre-fetch tag is picked by exact id, not family — a scoped
    /// card's post-fetch tag is always None, so handing it the default
    /// account's tag would freeze its capacity row as a perpetual swap.
    #[test]
    fn start_tag_matches_bare_default_ids_only() {
        assert_eq!(start_tag_for("claude", Some("a"), Some("b")), Some("a"));
        assert_eq!(start_tag_for("codex", Some("a"), Some("b")), Some("b"));
        assert_eq!(start_tag_for("claude@abcd1234", Some("a"), Some("b")), None);
        assert_eq!(start_tag_for("codex@abcd1234", Some("a"), Some("b")), None);
        // Scoped pairing: None/None is stable → a live poll advances.
        for id in ["claude@abcd1234", "codex@abcd1234"] {
            let stable =
                start_tag_for(id, Some("a"), Some("b")) == identity_tag_of(id, Some("x")).as_deref();
            assert!(may_advance(false, false, stable), "{id} must stay advanceable");
        }
    }

    /// A scoped id is stable only while fresh discovery still mints
    /// it; a re-signed dir produces a different id, so the in-flight
    /// poll can't be attributed to this card's account.
    #[test]
    fn scoped_identity_needs_to_survive_rediscovery() {
        let fresh: HashSet<String> =
            ["claude@aaaa1111", "claude@bbbb2222"].iter().map(|s| s.to_string()).collect();
        assert!(scoped_identity_stable("claude@aaaa1111", &fresh));
        assert!(!scoped_identity_stable("claude@cccc3333", &fresh));
        // Bare ids are covered by the pre/post tag rule — this helper
        // never gates them, even with an empty discovery.
        assert!(scoped_identity_stable("claude", &fresh));
        assert!(scoped_identity_stable("claude", &HashSet::new()));
    }

    #[test]
    fn identity_tag_only_applies_to_bare_default_ids() {
        // Scoped cards already embed their account — no switch bound.
        assert_eq!(identity_tag_of("claude@abcd1234", Some("uuid-1")), None);
        assert_eq!(identity_tag_of("claude", Some("aaaa1111-bbbb")), Some("aaaa1111".into()));
        assert_eq!(identity_tag_of("claude", None), None);
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

    /// The usage fetch and the spend scan run concurrently — the poll
    /// that first sees 100% may be holding totals from a scan whose
    /// files were read before the limit was reached. The cycle waits
    /// for a scan STARTED at-or-after the hit, then seals with ITS
    /// totals.
    #[test]
    fn observing_100_waits_for_a_scan_started_after_the_hit() {
        let e = poll(&Entry::default(), T0, 40.0, Some((40.0, 400_000_000.0)));
        let e = poll(&e, T0, 100.0, Some((218.0, 1_300_000_000.0)));
        let cur = e.current.as_ref().unwrap();
        assert_eq!(cur.status, Status::Active, "stale scan → not sealed");
        assert_eq!(cur.hit_full_at_ms, Some(T0 + 1_000));
        assert_eq!(cur.peak_pct, 100.0);
        // A pending cycle still estimates like an active one — at 100%
        // the estimate is simply the totals.
        assert_eq!(estimate(cur), Some((218.0, 1_300_000_000.0)));
        let m = metric(&e).unwrap();
        assert!(m.value.as_deref().unwrap().starts_with('≈'));

        // The scan that started after the hit seals the cycle — with
        // its newer totals — and nothing reprices it afterwards.
        let e = update(
            &e, T0, T0 + WEEK, 100.0,
            Some(wt(230.0, 1_400_000_000.0, T0 + 2_000)),
            T0 + 3_000,
        );
        let cur = e.current.as_ref().unwrap();
        assert_eq!(cur.status, Status::Observed);
        assert_eq!(cur.cost, 230.0);
        assert_eq!(cur.tokens, 1_400_000_000.0);
        assert_eq!(cur.observed_at_ms, Some(T0 + 3_000));

        let e2 = update(
            &e, T0, T0 + WEEK, 100.0,
            Some(wt(300.0, 9_000_000_000.0, T0 + 4_000)),
            T0 + 5_000,
        );
        let cur2 = e2.current.as_ref().unwrap();
        assert_eq!(cur2.cost, 230.0);
        assert_eq!(cur2.tokens, 1_400_000_000.0);
        // An observed row drops the ≈ — the numbers are real.
        let m = metric(&e2).unwrap();
        assert!(!m.value.as_deref().unwrap().starts_with('≈'));
        let detail: Value = serde_json::from_str(m.detail.as_deref().unwrap()).unwrap();
        assert!(detail["current"]["est_cost"].is_null());
    }

    /// A scan that STARTED before the hit can't seal even if it
    /// finished after — it may have read this card's files before the
    /// limit was reached.
    #[test]
    fn scan_started_before_the_hit_cannot_seal() {
        let e = poll(&Entry::default(), T0, 40.0, Some((40.0, 400_000_000.0)));
        let e = update(
            &e, T0, T0 + WEEK, 100.0,
            Some(wt(218.0, 1_300_000_000.0, T0 - 5_000)),
            T0 + 1_000,
        );
        assert_eq!(e.current.as_ref().unwrap().status, Status::Active);
        // A scan started after the hit seals, with its newer totals.
        let e = update(
            &e, T0, T0 + WEEK, 100.0,
            Some(wt(230.0, 1_400_000_000.0, T0 + 2_000)),
            T0 + 3_000,
        );
        let cur = e.current.as_ref().unwrap();
        assert_eq!(cur.status, Status::Observed);
        assert_eq!(cur.cost, 230.0);
    }

    /// A scan with a swallowed read gap still feeds the live estimate —
    /// but a permanent sample only seals on a COMPLETE scan.
    #[test]
    fn an_incomplete_scan_updates_but_cannot_seal() {
        let e = poll(&Entry::default(), T0, 40.0, Some((40.0, 400_000_000.0)));
        let e = update(
            &e, T0, T0 + WEEK, 100.0,
            Some(WindowTotals {
                cost: 218.0, tokens: 1_300_000_000.0,
                scan_started_ms: T0 + 2_000, complete: false,
            }),
            T0 + 3_000,
        );
        let cur = e.current.as_ref().unwrap();
        assert_eq!(cur.status, Status::Active, "gapped scan can't seal");
        assert_eq!(cur.cost, 218.0, "totals still update the estimate");
        // The next complete post-hit scan seals.
        let e = update(
            &e, T0, T0 + WEEK, 100.0,
            Some(wt(230.0, 1_400_000_000.0, T0 + 4_000)),
            T0 + 5_000,
        );
        assert_eq!(e.current.as_ref().unwrap().status, Status::Observed);
        assert_eq!(e.current.as_ref().unwrap().cost, 230.0);
    }

    /// When the totals on the very poll that sees 100% already come
    /// from a complete scan started at-or-after that moment, the cycle
    /// seals immediately.
    #[test]
    fn observing_100_seals_same_poll_when_the_scan_is_newer() {
        let e = update(
            &Entry::default(), T0, T0 + WEEK, 100.0,
            Some(wt(218.0, 1_300_000_000.0, T0 + 1_000)),
            T0 + 1_000,
        );
        let cur = e.current.as_ref().unwrap();
        assert_eq!(cur.status, Status::Observed);
        assert_eq!(cur.cost, 218.0);
        assert_eq!(cur.observed_at_ms, Some(T0 + 1_000));
    }

    /// A window that rolls over while a 100% seal is still pending is
    /// archived as observed but UNSEALED — it reached 100%, yet no
    /// post-hit scan has confirmed the money: observed_at stays empty,
    /// hit_full_at survives for resolve_pending's retries.
    #[test]
    fn rollover_while_pending_archives_as_finalizing() {
        let e = poll(&Entry::default(), T0, 100.0, Some((218.0, 1_300_000_000.0)));
        assert_eq!(e.current.as_ref().unwrap().status, Status::Active);
        let e = poll(&e, T0 + WEEK, 2.0, Some((0.5, 5_000_000.0)));
        let h = &e.history[0];
        assert_eq!(h.status, Status::Observed);
        assert_eq!(h.observed_at_ms, None);
        assert_eq!(h.hit_full_at_ms, Some(T0 + 1_000));
        assert_eq!(h.cost, 218.0);
    }

    /// resolve_pending retries a pending history cycle against newer
    /// scans: a COMPLETE scan started after the hit seals it with
    /// bounded totals; a pre-hit or gapped scan only refines the
    /// display; once the start hour leaves retention it gives up as
    /// incomplete.
    #[test]
    fn resolve_pending_seals_then_gives_up() {
        let e = poll(&Entry::default(), T0, 100.0, Some((218.0, 1_300_000_000.0)));
        let e = poll(&e, T0 + WEEK, 2.0, Some((0.5, 5_000_000.0)));
        let c = e.history[0].clone();
        assert!(is_pending(&c));
        assert_eq!(c.hit_full_at_ms, Some(T0 + 1_000));

        // One day after the window's end the start hour is still
        // retained — a post-hit complete scan seals.
        let day_ms = 86_400_000;
        let sealed = resolve_pending(&c, Some(wt(240.0, 1_500_000_000.0, T0 + 2_000)), c.end_ms + day_ms);
        assert_eq!(sealed.status, Status::Observed);
        assert_eq!(sealed.observed_at_ms, Some(c.end_ms + day_ms));
        assert_eq!(sealed.cost, 240.0);
        assert_eq!(sealed.tokens, 1_500_000_000.0);

        // A scan started before the hit → stays pending; its totals
        // still refresh the display.
        let still = resolve_pending(&c, Some(wt(999.0, 9e9, T0 - 1)), T0 + 9_000);
        assert!(is_pending(&still));
        assert_eq!(still.cost, 999.0);
        // A gapped post-hit scan can't confirm either.
        let gapped = resolve_pending(
            &c,
            Some(WindowTotals {
                cost: 300.0, tokens: 2e9,
                scan_started_ms: T0 + 5_000, complete: false,
            }),
            T0 + 9_000,
        );
        assert!(is_pending(&gapped));
        assert_eq!(gapped.cost, 300.0, "display totals update anyway");

        // Three days after end the start hour is gone (9-day retention
        // − 7-day window ≈ 2 days of retry room) — even a post-hit
        // complete scan can't seal anymore.
        let gave_up = resolve_pending(
            &c,
            Some(wt(240.0, 1_500_000_000.0, c.end_ms + 3 * day_ms)),
            c.end_ms + 3 * day_ms,
        );
        assert_eq!(gave_up.status, Status::Incomplete);

        // A sealed cycle is immune to resolve_pending.
        let immune = resolve_pending(&sealed, Some(wt(1.0, 1.0, 0)), T0 + 9_000);
        assert_eq!(immune.cost, 240.0);
    }

    /// Only SEALED observed weeks are evidence — a finalizing one keeps
    /// its numbers out of the average until a post-hit scan confirms.
    #[test]
    fn average_excludes_pending_observed_weeks() {
        let mut e = Entry::default();
        // Week 1 seals in place (scan stamp = its poll's now).
        e = update(&e, T0, T0 + WEEK, 100.0, Some(wt(200.0, 1_000_000_000.0, T0)), T0);
        // Week 2 hits 100% with a pre-hit scan → pending; week 3's
        // rollover archives it finalizing.
        e = update(&e, T0 + WEEK, T0 + 2 * WEEK, 100.0, Some(wt(260.0, 1_600_000_000.0, T0)), T0 + WEEK);
        e = update(&e, T0 + 2 * WEEK, T0 + 3 * WEEK, 10.0, Some(wt(20.0, 0.0, T0)), T0 + 2 * WEEK);
        assert!(is_pending(&e.history[0]));
        let (_, _, n) = observed_average(&e).unwrap();
        assert_eq!(n, 1, "the finalizing week isn't evidence yet");
        let m = metric(&e).unwrap();
        let detail: Value = serde_json::from_str(m.detail.as_deref().unwrap()).unwrap();
        assert_eq!(detail["avg"]["n"], 1);
        // The finalizing row still ships to the frontend as observed
        // with no observed_at — the popover labels it "finalizing".
        assert_eq!(detail["history"][0]["status"], "observed");
        assert!(detail["history"][0]["observed_at_ms"].is_null());
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
        // Pending at rollover still lands in history as observed.
        let e = poll(&Entry::default(), T0, 100.0, Some((218.0, 1_300_000_000.0)));
        let e = poll(&e, T0 + WEEK, 2.0, Some((0.5, 5_000_000.0)));
        assert_eq!(e.history[0].status, Status::Observed);
        assert_eq!(e.history[0].cost, 218.0);
    }

    #[test]
    fn reset_jitter_within_thirty_minutes_is_the_same_cycle() {
        let e = poll(&Entry::default(), T0, 40.0, Some((40.0, 400_000_000.0)));
        let e = update(&e, T0 + 20 * 60_000, T0 + WEEK + 20 * 60_000, 41.0, Some(wt(41.0, 410_000_000.0, T0)), T0 + 2_000);
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
            e = update(&e, T0 + w * WEEK, T0 + (w + 1) * WEEK, 10.0, Some(wt(w as f64, 0.0, T0)), T0);
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
        // observed at $260/1.6B, week 4 (current) active. Scans stamp
        // at-or-after their poll so the observed weeks seal in place.
        e = update(&e, T0, T0 + WEEK, 100.0, Some(wt(200.0, 1_000_000_000.0, T0)), T0);
        e = update(&e, T0 + WEEK, T0 + 2 * WEEK, 60.0, Some(wt(150.0, 0.0, T0)), T0);
        e = update(&e, T0 + 2 * WEEK, T0 + 3 * WEEK, 100.0, Some(wt(260.0, 1_600_000_000.0, T0)), T0);
        e = update(&e, T0 + 3 * WEEK, T0 + 4 * WEEK, 10.0, Some(wt(20.0, 0.0, T0)), T0);
        let (cost, tokens, n) = observed_average(&e).unwrap();
        assert_eq!(n, 2);
        assert!((cost - 230.0).abs() < 1e-9);
        assert!((tokens - 1_300_000_000.0).abs() < 1e-3);
        let m = metric(&e).unwrap();
        let detail: Value = serde_json::from_str(m.detail.as_deref().unwrap()).unwrap();
        assert_eq!(detail["avg"]["n"], 2);
    }

    /// The running cycle counts toward the average as soon as it's
    /// observed — a sample doesn't wait for rollover into history.
    #[test]
    fn average_includes_the_current_observed_cycle() {
        let mut e = Entry::default();
        e = update(&e, T0, T0 + WEEK, 100.0, Some(wt(200.0, 1_000_000_000.0, T0)), T0);
        e = update(&e, T0 + WEEK, T0 + 2 * WEEK, 100.0, Some(wt(260.0, 1_600_000_000.0, T0 + WEEK)), T0 + WEEK);
        assert_eq!(e.history.len(), 1);
        assert_eq!(e.current.as_ref().unwrap().status, Status::Observed);
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

    /// A reset credit spent mid-week restarts the window a few minutes
    /// forward with usage back near zero — a forward shift plus a drop
    /// is a new cycle, not jitter, and the old one archives incomplete.
    #[test]
    fn forward_shift_with_a_usage_drop_starts_a_new_cycle() {
        let e = poll(&Entry::default(), T0, 8.0, Some((5.0, 50_000_000.0)));
        let e = update(
            &e, T0 + 20 * 60_000, T0 + WEEK + 20 * 60_000, 0.5,
            Some(wt(0.0, 0.0, T0)), T0 + 2_000,
        );
        assert_eq!(e.history.len(), 1);
        assert_eq!(e.history[0].status, Status::Incomplete);
        assert_eq!(e.history[0].peak_pct, 8.0);
        assert_eq!(e.current.as_ref().unwrap().start_ms, T0 + 20 * 60_000);
    }

    #[test]
    fn forward_shift_without_a_material_drop_is_the_same_cycle() {
        let e = poll(&Entry::default(), T0, 8.0, Some((5.0, 50_000_000.0)));
        // +20 min, usage unchanged or rising → jitter, same cycle.
        let e = update(
            &e, T0 + 20 * 60_000, T0 + WEEK + 20 * 60_000, 8.0,
            Some(wt(5.0, 50_000_000.0, T0)), T0 + 2_000,
        );
        assert!(e.history.is_empty());
        let e = update(
            &e, T0 + 25 * 60_000, T0 + WEEK + 25 * 60_000, 9.0,
            Some(wt(5.0, 50_000_000.0, T0)), T0 + 3_000,
        );
        assert!(e.history.is_empty());
        // A small dip inside the slack isn't a restart either.
        let e = update(
            &e, T0 + 28 * 60_000, T0 + WEEK + 28 * 60_000, 8.5,
            Some(wt(5.0, 50_000_000.0, T0)), T0 + 4_000,
        );
        assert!(e.history.is_empty());
    }

    #[test]
    fn backward_shift_with_a_drop_stays_the_same_cycle() {
        let e = poll(&Entry::default(), T0, 8.0, Some((5.0, 50_000_000.0)));
        // The provider recomputed the start EARLY with lower usage —
        // a window can't have restarted in the past, so don't split.
        let e = update(
            &e, T0 - 10 * 60_000, T0 + WEEK - 10 * 60_000, 0.5,
            Some(wt(0.0, 0.0, T0)), T0 + 2_000,
        );
        assert!(e.history.is_empty(), "backward shift + drop is jitter");
    }

    /// A default card keeps `claude`/`codex` across sign-ins — the
    /// ledger key separates accounts; scoped cards embed theirs.
    #[test]
    fn ledger_key_separates_default_accounts() {
        assert_eq!(
            ledger_key_from("claude", Some("b3f1c2d4-9a8b-4c5d-8e9f-aabbccddeeff")),
            "claude|b3f1c2d4"
        );
        // An identity shaped like an email is truncated, never stored raw.
        assert_eq!(ledger_key_from("codex", Some("user@example.com")), "codex|user@exa");
        // Scoped ids already carry the account — no suffix needed.
        assert_eq!(
            ledger_key_from("claude@b3f1c2d4", Some("b3f1c2d4-9a8b")),
            "claude@b3f1c2d4"
        );
        // Identity unreadable → bare id, so a transient failure can't
        // orphan the ledger mid-session.
        assert_eq!(ledger_key_from("claude", None), "claude");
        assert_eq!(ledger_key_from("codex", Some("-")), "codex");
    }

    /// Restored and failed-attempt snapshots replay older polls — only
    /// a live one may advance the ledger.
    #[test]
    fn may_advance_blocks_restored_snapshots() {
        assert!(may_advance(false, false, true));
        assert!(!may_advance(true, false, true));
        assert!(!may_advance(false, true, true));
        assert!(!may_advance(true, true, true));
        // A mid-refresh sign-in swap blocks even a fresh live poll —
        // it can't be attributed to a known account.
        assert!(!may_advance(false, false, false));
        assert!(!may_advance(true, false, false));
        assert!(!may_advance(false, true, false));
        assert!(!may_advance(true, true, false));
    }

    /// A write is owed on any ledger change, and keeps being owed after
    /// a failure until one lands — an observed sample exists only in
    /// quota_cycles.json, so a dropped write loses it permanently.
    #[test]
    fn persist_gate_retries_until_a_write_lands() {
        assert!(persist_needed(true, false), "a change is always a write");
        assert!(persist_needed(false, true), "a dirty flag retries unchanged polls");
        assert!(persist_needed(true, true));
        assert!(!persist_needed(false, false), "steady state writes nothing");
    }
}
