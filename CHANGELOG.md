# Changelog

## Unreleased

### New
- **Wave 12 provider pack** — ten providers whose CLIs/tools leave tokens on
  disk or take an API key (provider research credit: steipete/CodexBar, MIT):
  Codebuff (credits + weekly, CLI login or key), Kilo (credit blocks +
  Kilo Pass, CLI login or key), Kiro (experimental CLI scrape of
  `kiro-cli /usage`), Amp (CLI `amp usage`, or API token — free-tier
  replenish ETA), Vertex AI (gcloud ADC identity + project), AWS Bedrock
  (native SigV4 → Cost Explorer monthly spend, optional PANE_BEDROCK_BUDGET
  progress bar, env keys or AWS_PROFILE via the AWS CLI), Poe (point
  balance), Chutes (4-hour + monthly quotas), Warp (request credits +
  bonus grants), Crof (daily requests + credit balance). 28 providers total.

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
