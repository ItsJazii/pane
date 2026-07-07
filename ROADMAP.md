# Roadmap — full Mac parity (and beyond)

Feature audit vs robinebers/openusage @ 4d75562 (2026-07-06). Waves are
ordered by dependency: the pace engine (Wave 1) feeds notifications
(Wave 2); structured metric data from Wave 1 also unlocks Wave 4's
toggles. Each wave ends with a shipped, installed build.

## Wave 1 — Pace engine (the brain)
- [x] Structured metric data: add `resets_at` (ms) + `period_ms` to Metric,
      emitted by every provider (replaces the preformatted reset string).
- [x] Burn-rate math: used% vs elapsed% of window → projection at reset.
- [x] Verdicts: blue (≥10% spare projected), yellow (<10% spare, "~3% spare"
      note), red (projected to run out — flame + "Limit in 3h 5m"),
      "Limit reached" when actually spent. No-reset metrics color by level
      (yellow ≥80% used, red ≤10% left).
- [x] Even-pace tick on the bar; hover shows projection at reset.

## Wave 2 — Notifications (Windows toasts)
- [x] tauri-plugin-notification + permission flow.
- [x] Alerts: Almost Out (<10% left), Cutting It Close (projected thin),
      Will Run Out (projected over) — once per metric per reset period,
      only when a quota *worsens* while running. State in config dir.
- [x] Three Settings switches, default off (like the Mac).

## Wave 3 — Usage Trend + spend depth
- [x] 30-day per-day token bar chart per provider (spend engine already
      buckets by day — expose the series, draw SVG bars).
- [x] Hover: peak day, date range, source note.
- [x] Per-model spend breakdown (extend spend engine to aggregate by model;
      hover a spend row → ranked model list with share bars, "Other" tail).
- [x] Total Spend polish: persist tab choice, hover exact total, ⓘ listing
      contributors, minimum visible sliver for tiny spenders.

## Wave 4 — Interaction polish
- [x] Click headline → flip Used ⇄ Left everywhere (persisted).
- [x] Click reset label → flip countdown ⇄ exact time (persisted).
- [x] 30-second local tick: countdowns/pace re-render between refreshes.
- [x] Time format setting (Auto / 12h / 24h).

## Wave 5 — Customize
- [x] Per-metric show/hide.
- [x] Always Visible vs On Demand (caret expander on cards, state persists).
- [x] Drag-to-reorder providers and metrics (HTML5 drag & drop).
- [x] Stars (≤2 per provider) drive the tray strip, replacing the plain
      trayProviders picker; strip follows customize order.
- [x] Per-provider Reset and Reset All (with confirm).
- [ ] Stretch: Ctrl+Z undo for customization steps. (deferred)

## Wave 6 — Platform features
- [x] Provider quick links (Status / Console / Dashboard buttons per card).
- [x] First-launch detection: fresh installs enable only providers whose
      local credentials exist.
- [x] Share screenshot: render a branded PNG of a card (canvas) → clipboard.
- [x] Local HTTP API on 127.0.0.1:6736 (GET /v1/usage, /v1/usage/:id).
- [x] Theme setting: System / Light / Dark (light palette via CSS vars).
- [x] Density: Default / Compact.
- [x] Global shortcut to toggle the popover (tauri-plugin-global-shortcut).
- [x] Proxy setting for all provider requests.

## Wave 7 — Antigravity (research project)
- [x] Discover Antigravity's local language-server process on Windows
      (process scan → ports + CSRF token from its command line).
- [x] Call RetrieveUserQuotaSummary / GetUserStatus locally (method names
      and parsing ported from the Mac source).
- [x] Fallback chain like the Mac: quota summary → legacy endpoints.

## Wave 8 — Ship v0.2.0
- [x] Version bump, changelog, rebuild installers, silent reinstall.
- [ ] (Parked until user says go: GitHub release, auto-updater, winget.)

All waves shipped 2026-07-07 as v0.2.0. Remaining backlog: Ctrl+Z undo in
Customize, GitHub/winget publishing, OpenCode official balance API when it
ships, Cursor server-side spend.

## Deliberately not ported
- Anonymous telemetry — ours stays zero-telemetry.
