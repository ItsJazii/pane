# Local HTTP API

Pane serves your usage as JSON so your own scripts, widgets, and overlays
can read it.

Sub2API cards use `GET /v1/usage/sub2api@<key-id>` and are also included
in the collection. Each key stays independent even if its wallet or
subscription values match another key. These entries additionally expose
`status`, `stale`, `error`, and `warning` so failed refreshes cannot make
historical data look current. Progress entries retain display amounts in
`value`/`subtitle` as well as the percentage. Other providers keep their
existing wire format.

This reads Pane's published snapshots and never initiates a remote usage
request. Disabled or deleted keys return 404 and disappear from the
collection; rotating a secret or changing a site address clears the old
context before publishing again. Renaming preserves the ID and updates
`displayName`. Empty sites have no snapshot. Only display fields are
included: no secret or fragment, origin, Dashboard URL, configuration
object, arbitrary remote error text, or raw response.

```
GET http://127.0.0.1:6736/v1/usage          # all enabled providers
GET http://127.0.0.1:6736/v1/usage/:id      # one provider (e.g. /claude, /onenewapi@<key-id>)
```

Wire format (compatible with the macOS OpenUsage API):

```json
[{
  "providerId": "claude",
  "displayName": "Claude",
  "plan": "Max",
  "fetchedAt": "2026-07-08T01:30:00Z",
  "lines": [{
    "type": "progress",
    "label": "Session",
    "used": 22.0,
    "limit": 100,
    "format": { "kind": "percent" },
    "resetsAt": "2026-07-08T04:39:59Z",
    "periodDurationMs": 18000000
  }]
}]
```

## Security posture

- **Loopback only.** Binds `127.0.0.1` — nothing on your network can
  reach it.
- **Usage numbers only.** Snapshots of what the dashboard shows — never
  credentials, tokens, or keys. One/New API entries use the full snapshot
  id (`onenewapi@<key-id>`) as `providerId` and the card title as
  `displayName`; dashboard URL, origin, site id, and secrets are omitted.
- **No CORS headers.** Unlike the macOS app (which sends
  `Access-Control-Allow-Origin: *` and documents that any web page can
  read your usage), Pane sends no CORS headers — so browsers block web
  pages from reading this API. PowerShell, curl, Rainmeter, and native
  apps are unaffected; CORS only constrains browsers.
- **Loopback Host headers only.** Requests whose `Host` header isn't a
  loopback spelling (`127.0.0.1`, `localhost`, or `[::1]`, with or
  without `:6736`) get `403 {"error": "forbidden_host"}`. This blocks
  DNS rebinding — a malicious page pointing its own hostname at
  127.0.0.1 to sidestep the browser's cross-origin rules. Plain
  scripts are unaffected (curl and PowerShell send a loopback Host
  automatically; a missing Host header is also fine), but if you call
  the API through a hosts-file alias or a proxy that rewrites Host,
  use one of the loopback spellings instead.
- **No authentication.** Any program running as your Windows user can
  read this API — that is what makes zero-config widgets and scripts
  possible, and it is a deliberate trade-off. What such a program gets is
  usage percentages and reset times; your credentials are never served.
  (A local process that could steal anything meaningful could also read
  the CLIs' credential files directly — the API adds no new exposure.)
- If port 6736 is already taken, the API is silently unavailable for that
  session; everything else works normally.
