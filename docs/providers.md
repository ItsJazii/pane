# Providers: exactly what Pane reads and calls

One section per provider: which credentials are read from your PC, which
endpoints they are sent to, and what comes back. Each provider's code
lives in [`src-tauri/src/providers/`](../src-tauri/src/providers/) in a
file of the same name — this page is the plain-English version of that
code.

Ground rules that apply to every provider:

- A credential is only ever sent to **its own vendor's API**, over HTTPS.
- If no credential is found, the provider shows a "connect me" hint (new
  installs auto-disable everything undetected except Claude and Codex).
- Expired OAuth tokens are refreshed against the vendor's own token
  endpoint and written back to the CLI's credential file, keeping the CLI
  signed in — identical to what the CLI does itself.

---

## Claude (Claude Code)

- **Reads:** `%USERPROFILE%\.claude\.credentials.json` (honors
  `CLAUDE_CONFIG_DIR`) — the OAuth token Claude Code saved when you logged
  in.
- **Calls:** `api.anthropic.com/api/oauth/usage` (usage windows);
  `platform.claude.com/v1/oauth/token` (refresh, written back).
- **Shows:** Session + Weekly windows, per-model weeklies, Extra Usage
  overage; local spend from `~\.claude\projects\` logs.

## Codex (Codex CLI)

- **Reads:** `%USERPROFILE%\.codex\auth.json`.
- **Calls:** `chatgpt.com/backend-api/wham/usage` (limits, Spark windows,
  credits); `.../wham/rate-limit-reset-credits` (reset credits, and
  `/consume` only when you click Use on a credit); OpenAI token refresh.
- **Shows:** Session/Weekly, Spark windows, credit balance, redeemable
  reset credits; local spend from `~\.codex\sessions\` logs.

## Cursor

- **Reads:** Cursor's local state database
  (`%APPDATA%\Cursor\User\globalStorage\state.vscdb` — copied before
  reading, never modified).
- **Calls:** `cursor.com` / `api2.cursor.sh` usage APIs; the dashboard's
  usage-events CSV export (for spend).
- **Shows:** credits, usage meters, plan; per-day spend.

## OpenCode (Go plan)

- **Reads:** `%USERPROFILE%\.local\share\opencode\opencode.db` (copied
  before reading) — message costs your own OpenCode history already
  contains.
- **Calls:** nothing — OpenCode has no public usage API yet; usage is
  computed locally against documented plan limits.

## GitHub Copilot

- **Reads:** gh CLI / Copilot tokens from Windows Credential Manager
  (`gh:github.com:<user>`) or legacy `hosts.yml` files.
- **Calls:** `api.github.com/copilot_internal/user`.
- **Shows:** credits/quota and plan.

## Grok (Grok CLI)

- **Reads:** `%USERPROFILE%\.grok\auth.json`.
- **Calls:** `cli-chat-proxy.grok.com/v1/billing` + settings; `auth.x.ai`
  token refresh (written back).
- **Shows:** weekly pool, pay-as-you-go cap badge; local spend from
  `~\.grok\logs\`.

## Devin (Devin CLI)

- **Reads:** `%APPDATA%\devin\credentials.toml`;
  `%APPDATA%\devin\cli\sessions.db` (+ WAL/SHM sidecars, copied before
  reading) for local spend.
- **Calls:** Devin's `GetUserStatus` RPC.
- **Shows:** weekly/daily quota, extra balance, plan; local spend from
  Devin CLI sessions (cloud Devin sessions bill ACUs and keep no local
  logs, so they can't be priced).

## MiniMax

- **Reads:** pasted key (Settings), `MINIMAX_API_KEY`, or
  `%USERPROFILE%\.minimax\config.yaml`.
- **Calls:** `api.minimax.io/v1/token_plan/remains` (+ regional fallbacks).
- **Shows:** 5-hour Session + Weekly plan windows.

## OpenRouter

- **Reads:** pasted key, `OPENROUTER_API_KEY`, or the key OpenCode stores.
- **Calls:** `openrouter.ai/api/v1/credits` and `/key`.
- **Shows:** balance, credits meter, key limit.

## Z.ai

- **Reads:** pasted key, env var, or the Z.ai CLI's key file.
- **Calls:** `api.z.ai` quota + subscription endpoints.
- **Shows:** Session/Weekly, monthly Web Searches quota, plan.

## Antigravity

- **Reads:** the running IDE's local language server (loopback), or the
  `gemini:antigravity` token in Windows Credential Manager.
- **Calls:** the local language-server RPC when the IDE runs; otherwise
  Google's Cloud Code quota API (`cloudcode-pa.googleapis.com`) with
  Google's own token refresh.
- **Shows:** Gemini + Claude pool windows, plan.

## DeepSeek / Moonshot / ElevenLabs / Venice-class key providers

- **Reads:** pasted key or env var only (`DEEPSEEK_API_KEY`,
  `MOONSHOT_API_KEY`/`KIMI_API_KEY`, `ELEVENLABS_API_KEY`).
- **Calls:** `api.deepseek.com/user/balance`;
  `api.moonshot.ai|cn/v1/users/me/balance`;
  `api.elevenlabs.io/v1/user/subscription`.
- **Shows:** balances / character quota with reset pacing.

## Ollama

- **Reads:** nothing.
- **Calls:** your own PC only — `127.0.0.1:11434` (`/api/version`,
  `/api/tags`, `/api/ps`).
- **Shows:** installed models, loaded models.

## Codebuff

- **Reads:** `%USERPROFILE%\.config\manicode\credentials.json` (the
  `codebuff login` file) or a pasted key.
- **Calls:** `codebuff.com/api/v1/usage` + `/api/user/subscription`.
- **Shows:** credits, weekly limit, plan.

## Kilo

- **Reads:** `%USERPROFILE%\.local\share\kilo\auth.json` or a pasted key.
- **Calls:** `app.kilo.ai/api/trpc/user.getCreditBlocks,kiloPass.getState`.
- **Shows:** credit blocks, Kilo Pass window, tier.

## Kiro *(experimental)*

- **Reads:** nothing directly — runs `kiro-cli chat --no-interactive
  /usage` (windowless) and parses its output.
- **Calls:** none of its own; the CLI talks to its own backend.
- **Shows:** credits, bonus credits, reset date, plan.

---

Provider request formats were researched from two MIT-licensed macOS
projects: [robinebers/openusage](https://github.com/robinebers/openusage)
and [steipete/CodexBar](https://github.com/steipete/CodexBar) — both
credited in [LICENSE](../LICENSE).
