# Security Policy

Pane reads the credential files that AI CLIs and editors keep on your PC.
That is a serious responsibility, and this page explains how we handle it
and how to reach us when something looks wrong.

## Reporting a vulnerability

**Please do not open a public issue for security problems.**

Use GitHub's private vulnerability reporting: go to the
[Security tab](https://github.com/ItsJazii/pane/security) → *Report a
vulnerability*. You'll get a response as fast as humanly possible for a
small open-source project — usually within a couple of days.

Please include: what you found, how to reproduce it, and what an attacker
could do with it.

## Security properties you can verify

All of this is auditable in the source — links go to the exact code.

- **Tokens never leave their lane.** Each provider's credential is sent
  only to that provider's own API over HTTPS
  ([`src-tauri/src/providers/`](src-tauri/src/providers/) — one module per
  provider; see [docs/providers.md](docs/providers.md) for the exact files
  read and endpoints called).
- **Zero telemetry.** There is no analytics SDK, no crash reporter, no
  "phone home" of any kind. The full list of network calls Pane can make
  is in [docs/privacy.md](docs/privacy.md).
- **The local HTTP API is loopback-only and CORS-locked.** It binds
  `127.0.0.1:6736`, serves usage numbers (never credentials), and sends no
  `Access-Control-Allow-Origin` header — so web pages you visit cannot
  read it from a browser ([`src-tauri/src/httpapi.rs`](src-tauri/src/httpapi.rs)).
- **Updates are cryptographically verified.** The auto-updater accepts
  only releases signed by the project's minisign key (held offline); the
  public key is baked into the app. The install script verifies the
  installer's SHA-256 against the release manifest and refuses to run on
  mismatch ([`install.ps1`](install.ps1)).
- **Links are restricted.** The app can only open `http(s)://` URLs in
  your browser — nothing that could launch a program.
- **API keys you paste** are stored in `%APPDATA%\Pane` on your PC,
  readable only by your Windows user, and sent only to their own vendor.

## Known limitations (honesty section)

- The installer is **not yet Authenticode-signed**, so SmartScreen warns
  on first run. Updates are minisign-verified regardless. A code-signing
  certificate is planned.
- Release binaries are currently built by the maintainer, not by public
  CI. Moving release builds to GitHub Actions (public build logs from the
  tagged source) is on the roadmap.
- Pane refreshes OAuth tokens and writes them back to the CLIs' own
  credential files (keeping your CLIs signed in). This means Pane has the
  same access to those accounts as the CLIs themselves — that's inherent
  to what the app does.

## Supported versions

Only the [latest release](https://github.com/ItsJazii/pane/releases/latest)
is supported. The auto-updater keeps installs current.
