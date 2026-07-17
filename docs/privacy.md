# Privacy

Pane is built on one rule: **your data is nobody's business, including
ours.** There is no Pane server, no account, and no telemetry.

## Every network call Pane can make

This is the complete list. Anything not listed here does not happen.

| Destination | When | What is sent |
|---|---|---|
| Each provider's own API (Anthropic, OpenAI/ChatGPT, cursor.com, GitHub, x.ai, Devin, MiniMax, OpenRouter, Z.ai, Google, DeepSeek, Moonshot, ElevenLabs, Codebuff, Kilo…) | Every refresh (default 1 min), only for providers you have enabled | That provider's own token/key, exactly as its official tool would send it. Full per-provider detail: [providers.md](providers.md) |
| `raw.githubusercontent.com` (LiteLLM), `models.dev`, `robinebers.github.io` | ~Daily | Anonymous GET for public model price tables (no identifying data) |
| `pane.jazii.dev/api/update` (falls back to `github.com/ItsJazii/pane/releases`) | On launch + every 4 h | Anonymous GET for the update manifest, carrying the app version. See "The update check" below for exactly what this counts. |
| `127.0.0.1:11434` (your own PC) | Every refresh, if Ollama is enabled | Local-only query of your Ollama server |

Notably absent: analytics, crash reporting, A/B flags, in-app event
tracking — none of it exists in the codebase. The Mac app this project
was inspired by ships PostHog telemetry (disclosed and toggleable); Pane
deliberately ports everything **except** that.

## The update check

Pane has to ask *somewhere* "is there a newer version?" — that request
existed from day one. As of 0.4.17 it goes to `pane.jazii.dev` (which
serves the same signed manifest; GitHub remains the automatic fallback,
and every update is still signature-verified against the key baked into
the app). The server counts, per day: **how many distinct installs
checked in, from which country, on which version.** That's the entire
list. Concretely:

- **No IP addresses are stored.** Uniqueness comes from a salted one-way
  hash folded into a [HyperLogLog](https://en.wikipedia.org/wiki/HyperLogLog)
  — a counter that can say "≈37 distinct installs today" but is
  mathematically incapable of listing them.
- **Country** is the two-letter code the CDN edge derives; nothing
  finer (no city, no region, no coordinates).
- **Version** is the `?v=` parameter the updater sends.
- Nothing else: no machine ids, no usernames, no usage data — your
  quotas, spend, and provider data never leave your PC, same as always.

The counting code is public in the site repo, and the request itself is
identical either way — the only thing that changed is who serves the
manifest first.

## What stays on your PC

- **Credentials**: read from the files the official CLIs already maintain
  (see [providers.md](providers.md)); pasted API keys live in
  `%APPDATA%\Pane\<provider>.json`. Sent only to their own vendor.
- **Refreshed OAuth tokens**: written back to the CLIs' own credential
  files so your tools stay signed in — same behavior as the CLIs
  themselves.
- **Usage snapshots & spend cache**: `%APPDATA%\Pane\` — cached locally so
  the app opens instantly; never uploaded.
- **Spend accounting**: computed by reading the CLIs' local log files on
  your disk. The logs never leave your machine; only the public price
  tables are downloaded.

## The local HTTP API

`http://127.0.0.1:6736/v1/usage` exists so your own scripts and widgets
can read your usage. It is loopback-only (nothing on your network can
reach it), serves usage numbers only (never credentials or keys), and
sends **no CORS headers** — so websites you visit cannot read it through
your browser. Details: [local-http-api.md](local-http-api.md).

## Verifying all of this

Pane is MIT-licensed and this repository is the entire codebase. Search
it: there is no analytics import, and every `http` call site lives either
in a provider module ([`src-tauri/src/providers/`](../src-tauri/src/providers/)),
the pricing engine ([`src-tauri/src/pricing.rs`](../src-tauri/src/pricing.rs)),
or the updater registration ([`src-tauri/src/lib.rs`](../src-tauri/src/lib.rs)).
