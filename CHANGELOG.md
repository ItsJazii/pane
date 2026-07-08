# Changelog

## Unreleased

### Fixed
- **Cursor Pro/Pro+/Ultra/Teams accounts now show real usage** — percent
  of the plan's included usage, Auto/API usage, on-demand spend, and
  credits, with the actual billing-cycle reset date, via the same API
  Cursor's own dashboard uses. Previously modern accounts showed only a
  meaningless "Requests this cycle: 0" from the legacy request-counter
  era (old request-based plans still fall back to it). Session tokens
  are auto-refreshed in memory when stale, and reading Cursor's login
  no longer fails when Cursor briefly holds a lock on its database.

## 0.4.4 — 2026-07-08

### Performance
- Background refreshes no longer rebuild the popover interface while
  it's hidden in the tray (~99% of the time) — rendering now happens
  once, at the moment you open Pane. Same for the 30-second countdown
  ticks. Less idle CPU, all day.
- New **Settings → General → "Liquid glass effects"** toggle (on by
  default). Turn it off on slower PCs: the glass refraction and blurs
  become clean flat surfaces, and the expensive lens machinery never
  even initializes — from the very first frame of a cold start.

## 0.4.3 — 2026-07-08

### New
- **Pane's new logo** 🎯 — the ring, everywhere: installer, app and
  taskbar icons, tray, popover sidebar, and share-card footers.
- **Update checks on every open** — the footer version stamp re-checks
  each time you open Pane and becomes a blue **⬆ Update** button when a
  new release is out; one click installs and restarts. (Replaces the
  floating update banner.)
- Party mode is now a triple-click on the sidebar logo away. 🎉

### Fixed
- Tray strip pairs now render logo-then-numbers, left to right, like
  the macOS original (Windows inserts new tray icons leftward).
- The update flow can no longer freeze at "Installing…" if a release
  disappears mid-install — it fails visibly with a retry button.

## 0.4.2 — 2026-07-08

**⚠ Installs of 0.4.1 and earlier: this release is signed with a new
key, so the in-app updater will decline it. Reinstall once via
`irm https://pane.jazii.dev/install.ps1 | iex` — updates then resume
normally.**

### Changed (security)
- **Breaking for browser-page consumers:** the local HTTP API no longer
  sends CORS headers, so web pages can no longer read
  `127.0.0.1:6736` through a browser — previously any website you visited
  could silently read your usage data. PowerShell, curl, Rainmeter, and
  native apps are unaffected (CORS only constrains browsers). If a
  legitimate browser integration needs access, open an issue — the plan
  is an opt-in origin allowlist, not a permissive default.
- Release binaries are now built and published by GitHub Actions from the
  pushed tag (public build logs), instead of on the maintainer's machine.
- The updater signing key was rotated to a passphrase-protected key
  (2026-07-08). Installs of 0.4.1 and earlier trust the old public key, so
  their auto-updater will decline the first release signed with the new
  key — reinstall once via `irm https://pane.jazii.dev/install.ps1 | iex`
  (or the release installer) and updates resume normally.
- **Webview hardening pass** (from a community security review): a strict
  Content-Security-Policy replaces the previous `csp: null`; the Tauri
  capability set is trimmed to core IPC only (the UI never used the
  opener/updater/process plugin APIs); `withGlobalTauri` is off; the
  pinned-metric dropdown is built via DOM instead of HTML strings.
- **Rust-side input validation:** `set_config` only accepts known config
  keys; tray-strip updates only accept known provider ids (also fixes
  unstarred tray icons of newer providers not being removed); pricing
  supplement alias rules are size- and count-capped.
- **Credential safety:** CLI credential files are copied to `*.pane-bak`
  before Pane writes a refreshed token back.

### Added
- SECURITY.md (private vulnerability reporting), docs/privacy.md (every
  network call), docs/providers.md (per-provider: files read + endpoints
  called), docs/local-http-api.md, CONTRIBUTING.md.

### Fixed
- Share cards (⧉) now copy the card exactly as it looks on screen —
  donut, tabs, theme and all — framed with a Pane logo footer, instead
  of a simplified redrawn version that didn't match the UI.

## 0.4.1 — 2026-07-08

First-run and Customize fixes from fresh-install testing.

### Fixed
- Fresh installs now start with just Claude + Codex enabled (their
  "connect me" cards are the onboarding); a PC with zero detected AI
  tools no longer enables all 18 providers.
- Rapidly toggling several providers off kept only the last change —
  toggles now apply instantly and save through a serial delta queue.
- Disabled providers disappear immediately from the dashboard, the
  Total Spend donut, and the tray strip (previously they lingered until
  the next refresh).
- Total Spend shows a quiet "No spend data yet" card on machines whose
  CLIs haven't logged usage, instead of no card at all.

## 0.4.0 — 2026-07-07

### New
- **Three CLI-detected providers** (research credit: steipete/CodexBar, MIT):
  Codebuff (credits + weekly limit, `codebuff login` file or key), Kilo
  (credit blocks + Kilo Pass, CLI login file or key), and Kiro
  (experimental — reads `kiro-cli /usage`). **18 providers total.**
- **Auto-updater** — Pane checks GitHub releases on launch and every 4
  hours; a banner offers one-click download + restart. Updates are
  cryptographically signed and verified before install.
- **Deeper metric rows** (Mac-parity polish): Claude per-model weeklies
  from Anthropic's new `limits` API (Fable era) + Extra Usage overage
  dollars; Codex Spark / Spark Weekly windows + Extra Usage credit
  balance; Z.ai monthly Web Searches quota; Grok pay-as-you-go cap badge.
- **"Not started"** — untouched 5-hour session windows say so (with an
  explainer) instead of showing a countdown that hasn't begun; Codex's
  floored 1%-on-fresh-window quirk is normalized to a true zero.
- **Keyboard** — Esc backs out of Customize/Settings, Ctrl+R refreshes.

### Removed
- Deepgram, OpenAI, Venice, Poe, Chutes, Warp, Crof, Amp, Vertex AI, and
  AWS Bedrock providers — cut to keep the lineup focused on the AI coding
  tools people actually track. Saved layouts self-clean any retired ids.

### Fixed
- Terminal windows no longer flash during refreshes (provider CLI checks
  now run windowless or scan the filesystem instead of spawning `cmd`).
- Retired providers no longer linger as ghost rows in Customize.
- Startup crash at higher provider counts (fetch futures now heap-boxed).
- Clicking the tray icon always reopens on the main page, even if the app
  was left on Settings or Customize.

## 0.3.0

### Renamed to Pane
- The app is now **Pane** (formerly OpenUsage for Windows). Installs to
  `%LOCALAPPDATA%\Pane`; settings move automatically from
  `%APPDATA%\OpenUsage` to `%APPDATA%\Pane` on first launch — keys, layout,
  and caches all carry over.

### Accuracy (Wave 9)
- **Live model pricing** — per-model rates now come from LiteLLM, models.dev,
  and the OpenUsage pricing supplement (daily refresh, ETag caching, offline
  fallback) instead of a hardcoded table. Claude spend was overstated ~2.6×
  at old Opus rates; Codex fast-tier requests now get their real multiplier.
- **Unpriced events are excluded, not guessed** — models no catalog prices
  are left out of totals and flagged with ⚠ (count + model names on hover).
- **Cursor spend** — computed from the dashboard's usage-events CSV export,
  priced locally.
- **Codex dedupe** — archived session copies no longer double-count.
- **Backoff & cooldown** — failing providers are benched 60s (5 min for
  rate limits) while cached data is served with the reason on hover.
- **Reset all layouts** also re-detects installed AI tools.
- **Codex reset credits** — each banked credit shows its exact expiry and a
  Use button that redeems it (confirm-guarded, idempotent).
- Single-instance guard; popover reopens scrolled to the top.

### UI
- Auto-hiding liquid-glass sidebar (prasen.dev lens: SDF rim refraction,
  chromatic fringe) with magnetic minimap trail; glass footer with build
  stamp; full-window Customize and accordion Settings panels; ☀/☾ theme
  toggle with circular wipe; per-day trend tooltips; skeleton loading,
  staggered card entrances, and a full light-mode audit.

### New
- **Wave 11 provider pack** — seven providers that authenticate with a pasted
  API key (Settings → API keys) or nothing at all:
  DeepSeek (balance), Moonshot/Kimi (balance, .ai + .cn), ElevenLabs
  (character quota with reset pacing), Deepgram (project balances),
  OpenAI (org costs — needs an Admin key), Venice (USD/DIEM/VCU balances),
  and Ollama (local server: installed + loaded models, no key needed).
  Providers added by an update that have no credentials on this PC start
  disabled — enable them in Customize.
- **MiniMax provider** — Coding/Token Plan quota (5-hour Session + Weekly
  windows) via the same endpoint the official `mmx quota` command uses. Key
  auto-detected from the MiniMax CLI's config, `MINIMAX_API_KEY`, or the new
  Settings field.
- **Copilot CLI / modern gh detection** — GitHub tokens are now also read
  from Windows Credential Manager (`gh:github.com:<user>`), which is where
  current gh versions (and the Copilot CLI) keep them. hosts.yml-only setups
  keep working.
- **Motion pass** — cards slide in with a stagger and bars fill when the
  popover opens, skeleton shimmer while first data loads, hover elevation on
  cards, smooth caret/tab/button transitions. All entrance animations play
  only on open (never on background refreshes) and respect the system's
  reduced-motion setting.

### Fixed
- config.json parsing now tolerates a UTF-8 BOM and logs parse failures
  instead of silently resetting settings to defaults.

## 0.2.0 — 2026-07-07

Full feature parity with the macOS original, plus Windows-specific polish.

### New
- **Antigravity support** — reads quota from the IDE's local language server
  when it's running, and falls back to Google's Cloud Code API (token from
  Windows Credential Manager) when it isn't. Session / Weekly / Claude /
  Claude Weekly metrics plus plan name.
- **Provider quick links** — Status / Dashboard links at the bottom of each
  card, same targets as the Mac app.
- **Share cards** — hover a card and click ⧉ to copy it as a PNG image to the
  clipboard (works for Total Spend too).
- **Local HTTP API** — `GET http://127.0.0.1:6736/v1/usage` (and
  `/v1/usage/:providerId`) serves the latest snapshots in the Mac app's
  documented wire format. Scripts written for the Mac app work unchanged.
- **Appearance** — System / Light / Dark theme setting.
- **Compact layout** — tighter density option.
- **Global shortcut** — e.g. `Ctrl+Shift+U` to toggle the popover from
  anywhere.
- **Proxy** — optional `socks5://` / `http(s)://` outbound proxy.
- **First-launch detection** — a fresh install starts with only the providers
  that have credentials on the PC; the rest wait in Customize.

### Fixed
- The popover no longer sits on "Loading usage data…" while the spend engine
  scans session logs on a cold start — usage cards paint immediately and the
  Total Spend card fills in when the scan finishes.
- Last-good snapshots are now cached **on disk**, so a transient provider
  outage (or rate limit) right after an app restart shows amber "⚠ Outdated"
  data instead of an error card. Entries expire after 24 hours.
- Drag-and-drop in Customize works (Tauri's native drag interceptor disabled).

## 0.1.0 — 2026-07-06

First Windows release: 10 providers (Claude, Codex, Cursor, OpenCode,
Copilot, Grok, Devin, OpenRouter, Z.ai, Antigravity detection), local spend
engine with model breakdown and 30-day trend, pace projections, toast
notifications, tray strip with per-provider icons, Customize screen, NSIS
installer, autostart.
