# Pane

All your AI subscription limits in one liquid-glass tray popover for
Windows. Click the tray icon next to the clock and see, at a glance, how
much of your session/weekly limits each AI tool has left — with pace
projections, local spend accounting, and live tray-icon numbers.

Started as a Windows rebuild of the excellent
[OpenUsage for macOS](https://github.com/robinebers/openusage) by Robin
Ebers (MIT licensed), built from scratch with Tauri v2 + vanilla
TypeScript, and growing beyond usage tracking from there.

## Providers (18 and counting)

| Provider | Source of data |
|---|---|
| Claude (Claude Code) | `%USERPROFILE%\.claude\.credentials.json` + Anthropic usage API |
| Codex (Codex CLI) | `%USERPROFILE%\.codex\auth.json` + ChatGPT usage API, incl. reset-credit redemption |
| Cursor | Cursor's local state database + cursor.com API |
| OpenCode (Go plan) | Local `opencode.db` spend vs documented plan limits* |
| GitHub Copilot | Copilot editor login or GitHub CLI (Credential Manager) + GitHub API |
| Grok (Grok CLI) | `%USERPROFILE%\.grok\auth.json` + Grok billing API |
| Devin (Devin CLI) | `%APPDATA%\devin\credentials.toml` + GetUserStatus RPC |
| MiniMax | API key (Settings, env var, or CLI config) + token-plan API |
| OpenRouter | API key (Settings) or key stored by OpenCode |
| Z.ai | API key (Settings), CLI key file, or env var |
| Antigravity | Local language server, or Google Cloud Code API via Credential Manager |
| DeepSeek | API key (Settings) → balance |
| Moonshot (Kimi) | API key (Settings) → balance (global + CN endpoints) |
| ElevenLabs | API key (Settings) → character quota with reset pacing |
| Deepgram | API key (Settings) → project balances |
| OpenAI | Admin API key (Settings) → org costs (Today / 30 days) |
| Venice | API key (Settings) → USD/DIEM/VCU balances |
| Ollama | Local server on :11434 — installed + loaded models, no key |

*OpenCode has no public usage API yet
([anomalyco/opencode#10448](https://github.com/anomalyco/opencode/issues/10448));
usage is computed locally from this machine's OpenCode history, the same
data `opencode stats` uses.

Tokens are read from the files the official tools already maintain and are
sent only to their own vendor APIs. No telemetry, no middleman server.
Expired OAuth tokens are refreshed and written back, keeping the CLIs
signed in.

## Features

- **Pace projections** — colored bars and "will run out" warnings based on
  your burn rate within each reset window, plus optional Windows toasts.
- **Local spend** — Today / Yesterday / 30 Days donut with per-model
  breakdown and a 30-day trend, computed entirely from the CLIs' own logs
  and priced with live model rates (LiteLLM / models.dev, refreshed daily).
- **Codex reset credits** — see each banked credit's exact expiry and
  redeem it with one click.
- **Customize** (☰) — reorder, hide, or star metrics; stars become live
  tray icons (logo + number pairs). Ctrl+Z undoes layout changes.
- **Liquid glass UI** — SDF lens refraction on the auto-hiding sidebar and
  glass bars, magnetic minimap trail, circular day/night wipe.
- **Share cards** — hover a card, click ⧉, paste the PNG anywhere.
- **Quick links** — Status / Dashboard shortcuts on every card.
- **Local HTTP API** — `GET http://127.0.0.1:6736/v1/usage`, same wire
  format as the Mac app's documented API.
- **Appearance** — System / Light / Dark, plus a Compact density.
- **Global shortcut** — toggle the popover from anywhere (e.g. `Ctrl+Shift+U`).

## Settings (gear icon)

- Refresh interval (default: every 5 minutes)
- Start with Windows
- Which metric the tray icon displays
- Appearance, compact layout, global shortcut, time format
- Optional outbound proxy (`socks5://` or `http(s)://`)
- API keys (stored only on this PC in `%APPDATA%\Pane`)

## Development

Prerequisites: Node.js, Rust (stable-msvc), Visual Studio C++ Build Tools,
WebView2 (bundled with Windows 11).

```
npm install
npm run tauri dev     # run with hot reload
npm run tauri build   # produce installer (src-tauri/target/release/bundle)
```

## Credit & license

MIT — see [LICENSE](LICENSE).

The provider research — which credential files to read, which endpoints to
call — comes from the excellent macOS original:
[robinebers/openusage](https://github.com/robinebers/openusage) (MIT).
Pane is an independent project and is not affiliated with Robin Ebers or
any of the AI vendors listed. Provider names and logos belong to their
respective owners and are used only to identify the services.
