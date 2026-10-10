---
name: aetherium
description: Control a running aetherium SSH/SFTP client through its localhost REST API — discover targets, open shell and log-follow tabs, run commands on remote hosts, and read/replace/upload/download files. Use when asked to operate aetherium, work with one of its saved connection targets, tail a remote log, or transfer files through the app.
---

# aetherium — AI control via the local REST API

aetherium is a GUI SSH/SFTP client (gpui-based, Zed's UI framework).
While it runs, it exposes a **self-describing HTTP API on 127.0.0.1**
that drives the *visible UI*: every operation runs on the user's open
SSH sessions. There are no hidden connections — tabs opened via the API
appear on screen.

## Connecting

1. The app must be running.
2. Base URL: `http://127.0.0.1:48920` — the exact port is in the app's
   startup log and status bar (`AETHERIUM_API_PORT` overrides; fallback
   range 48920–48925). Try `GET /health` to test.
3. Auth: `Authorization: Bearer <token>` header on every request except
   `/` and `/health`. The token is in the app's config directory:
   - macOS/Linux: `~/.config/aetherium/api_token` or
     `~/Library/Application Support/aetherium/api_token`
   - Windows: `%APPDATA%\aetherium\api_token`
   Read the file; do not ask the user to paste the token.

## Quick start

```bash
TOKEN=$(cat ~/Library/Application\ Support/aetherium/api_token)
curl -s http://127.0.0.1:48920/                     # full endpoint docs (no auth)
curl -s -H "Authorization: Bearer $TOKEN" \
     http://127.0.0.1:48920/status                 # profiles + open tabs
```

Reply convention: `{"ok": true, ...}` or `{"ok": false, "error": "..."}`
with HTTP 200; auth/transport failures use 401/404/504.

## Endpoints

| Method | Path | Purpose |
|---|---|---|
| GET | `/health` | Liveness probe (no auth). |
| GET | `/` | Self-describing index: every endpoint with params and examples (no auth). |
| GET | `/status` | Saved profiles (names + `user@host:port` summaries) and open tabs with connection state. **Start here to discover targets.** |
| POST | `/sessions` | `{"target": "<profile name>"}` — opens a visible shell tab and connects. Idempotent: reconnects stale tabs. |
| POST | `/logs` | `{"target": …, "path": "/var/log/syslog"}` — opens a SnakeTail-style log-follow view (auto-waits for connect, up to 30 s). |
| POST | `/logs/collect` | `{"target": …}` — one-call diagnostics bundle: runs the built-in source set and any extras from `collect.toml`, replying with one entry per source. See below. |
| POST | `/exec` | `{"target": …, "command": "uptime", "timeout_secs": 60}` → `stdout`, `stderr`, `exit_status` (`*_base64` added for non-UTF-8 output). Requires a connected session. |
| GET | `/files?target=…&path=…` | List a remote directory (`entries`: name/path/is_dir/size/modified). |
| GET | `/file?target=…&path=…` | Download a file → `{"size": n, "content_base64": …}`. |
| PUT | `/file?target=…&path=…` | Replace/upload a file; **the request body is the new content** (binary-safe). |

## The standard flow

1. `GET /status` → pick the profile `name` for the host you need.
2. No connected tab for it? `POST /sessions` first, then wait for
   `"state": "connected"` on the next `/status` poll (a few seconds).
3. Run `POST /exec`, or do file operations. Errors like *"no connected
   session — POST /sessions first"* mean step 2 was skipped.
4. To watch a log: `POST /logs`, then let the user read the tab; re-run
   `POST /exec` for point-in-time checks.

## Python helper

```python
import base64, json, pathlib, urllib.request

TOKEN = pathlib.Path("~/Library/Application Support/aetherium/api_token").expanduser().read_text().strip()
BASE = "http://127.0.0.1:48920"

def api(method, path, body=None, raw=None):
    req = urllib.request.Request(BASE + path, method=method,
        data=raw if raw is not None else (json.dumps(body).encode() if body is not None else None))
    req.add_header("Authorization", f"Bearer {TOKEN}")
    if body is not None:
        req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req, timeout=120) as r:
        return json.load(r)

# print(os:=api("POST", "/exec", {"target": "raspberry3bplus", "command": "hostname"}))
# api("PUT", "/file?target=raspberry3bplus&path=/etc/motd", raw=b"hello\n")
```

## Log collection (`POST /logs/collect`)

One call pulls the common Linux diagnostics from a target into a single
JSON response — built for AI triage:

```bash
curl -s -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
     -d '{"target": "raspberry3bplus"}' \
     http://127.0.0.1:48920/logs/collect
```

Reply: `{"ok": true, "sources": [{"name", "kind", "ok", "content",
"bytes", "truncated", "error"}, …]}`.

- **Built-in sources**: `dmesg`, `journalctl -b --no-pager -n 2000`,
  and the files `/var/log/syslog`, `/var/log/messages`,
  `/var/log/kern.log`, `/var/log/auth.log`, `/var/log/daemon.log`,
  `/var/log/dmesg`.
- **Configurable extras**: `collect.toml` in the aetherium config dir
  adds per-user sources, e.g. to also grab an application log and a
  service status:

  ```toml
  files = ["/app/sovd/sovd.log"]
  commands = ["systemctl status sovd --no-pager"]
  ```

  With that file, the collection includes `dmesg`, the journal, the
  built-in /var/log files, **and** `/app/sovd/sovd.log` plus the
  `systemctl status` output — each as its own source entry. Restart the
  app after editing `collect.toml`.
- Sources run **sequentially** over the target's connected session (one
  exec channel at a time); each entry reports its own `ok`/`error`, a
  missing file or unsupported command never aborts the rest.
- `content` is capped at 512 KiB per source with `truncated: true` when
  the cap cut it; files are fetched via `cat | head -c 512289` so huge
  logs cannot flood memory.
- Requires a connected session (`POST /sessions` first). Only one
  collection can run at a time; the whole job times out after 300 s,
  each source after 45 s.
- This endpoint is read-only and never opens UI tabs — it only controls
  the existing session, per the API's contract.

## Notes and limits

- Operations affect the user's real sessions and files — destructive
  remote commands are the user's responsibility; confirm intent for
  anything irreversible.
- `/exec` timeout is per-request (1–900 s); long-running commands
  should be started with `nohup … &` and polled via output files.
- Transfers and log tabs opened via the API behave exactly like
  user-initiated ones (progress bar, tabs visible, closable).
- The API binds to localhost only; the bearer token protects against
  other local processes. Keep the token file private.

Source: the app repo (jFiedler24/aetherium), `src/api.rs` and
`docs/requirements/api.md`.
