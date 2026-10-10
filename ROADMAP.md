# Aetherium Roadmap

This file tracks planned features and implementation progress for the aetherium SSH/SFTP terminal.

## Legend

- `[ ]` Not started
- `[~]` In progress
- `[x]` Completed

---

## 1. Transfer progress UX

- [x] Backend emits `TransferStarted`, `TransferProgress` (with speed + ETA), `TransferDone`.
- [x] Status bar shows label, done/total, percentage, speed, and ETA.
- [x] Format byte counts as KB/MB/GB — done via existing `format_size`.
- [x] Render a graphical progress bar with percentage (cancel button beside it).
- [ ] Support multiple concurrent transfers (queue + per-transfer rows).
- [x] Add a "Cancel transfer" action — a shared cancel flag checked between chunks; partial files are removed.

Implementation notes: `TransferProgress` in [src/session.rs](src/session.rs) now
tracks a rolling 3-second sample window to estimate bytes/sec and ETA; the
`Event::TransferProgress` variant carries `bytes_per_second` and
`eta_seconds`. [src/ui.rs](src/ui.rs)'s `render_statusbar` renders
`label — done/total (pct%) — speed/s — ETA Xs`.

---

## 2. Security & trust

### 2.1 Encrypted password storage — DONE

- [x] AES-256-GCM encryption helper in [src/crypto.rs](src/crypto.rs).
- [x] Random 32-byte key stored at `~/.config/aetherium/key` (base64, 0600 on Unix).
- [x] `profiles.toml` now stores `encrypted_password` / `encrypted_passphrase`;
      `ProfileStore` transparently encrypts on save and decrypts on load.
- [x] In-memory `Profile`/`AuthMethod` API unchanged — no UI changes required.
- [x] Unit tests: `crypto::tests::round_trip`, `profiles::tests::toml_round_trip`.

### 2.2 `known_hosts` verification — DONE

- [x] `ClientHandler::check_server_key` now calls `russh::keys::check_known_hosts`.
- [x] Unknown hosts are trust-on-first-use: the key is learned via
      `russh::keys::known_hosts::learn_known_hosts` into `~/.ssh/known_hosts`.
- [x] A *changed* key for a known host is rejected (connection fails) instead
      of silently accepted, closing the v1 "accept anything" hole.
- [ ] Optional: surface a UI prompt distinguishing "new host" vs "key changed"
      instead of only a connection error.

---

## 3. Connection quality of life

### 3.1 Auto-detect default SSH keys — DONE

- [x] `detect_default_ssh_key()` in [src/session.rs](src/session.rs) checks
      `~/.ssh/id_ed25519`, `id_rsa`, `id_ecdsa` in that order.
- [x] An empty key-file path in a profile now falls back to the detected key
      instead of failing to load.

### 3.2 Login prompt for incomplete credentials — DONE

- [x] `needs_login_prompt()` in [src/ui.rs](src/ui.rs) flags a profile with an
      empty username, or (for password auth) an empty password.
- [x] `connect_profile()` now opens the profile form pre-filled instead of
      connecting blind when credentials are incomplete — matches the recents
      flow that already did this for unsaved history entries.

### 3.3 Key/agent-first auth fallback — DONE

- [x] For `AuthMethod::Password` profiles, `authenticate()` now tries
      ssh-agent, then a default key file, silently before sending the stored
      password — the password is only used if the server still needs it.
- [x] Explicit `KeyFile`/`Agent` profile selections are unchanged.

### 3.4 Cross-session command history — DONE

- [x] Persist last 200 terminal commands to `~/.config/aetherium/command_history.toml`.
- [x] Deduplicate consecutive identical commands.
- [x] `Shift+↑` / `Shift+↓` recall the history in the terminal (skipped in
      full-screen apps, where those keys belong to the app).


---

## 4. Remote file editing

### 4.1 Download → open → watch → upload workflow — DONE

- [x] Double-click (or context menu) downloads a remote file to a staged
      local temp copy and opens it in the OS default editor, VS Code
      (`start_remote_edit` in [src/ui.rs](src/ui.rs)).
- [x] `$AETHERIUM_EDITOR` overrides the OS default editor.
- [x] The temp copy is watched (mtime + size); on save the app asks with a
      dialog whether to sync the changes back to the device (upload on
      "Upload", staging cleaned up on "Ignore" or disconnect).

### 4.2 File association / tool mapping

- [ ] Add settings for mapping extensions (e.g., `.txt`, `.conf`) to applications.
- [ ] Build a simple gpui settings panel to manage associations.
- [x] Use associations when opening remote files — partially covered by
      `$AETHERIUM_EDITOR` and the explicit "Open in VS Code" action.

---

## 5. CLI companion

- [x] Superseded by the REST API for now: `GET /status`, `POST /sessions`,
      `POST /exec`, `GET/PUT /file` cover the CLI use cases without a
      separate crate (see [src/api.rs](src/api.rs) and the aetherium skill).
- [ ] Add a new `crates/aetherium-cli/` workspace member.
- [ ] Commands (in priority order):
  - [ ] `profiles` — list saved profiles as JSON.
  - [ ] `export-ssh-config` — generate an `~/.ssh/config` snippet from profiles.
  - [ ] `connect <profile>` — open a terminal session via the existing backend.
  - [ ] `exec <profile> <command>` — run a remote command.
  - [ ] `cp <profile:path> <local>` / `cp <local> <profile:path>` — file transfers.

---

## 6. Distribution & CI

- [x] Windows executable now embeds an app icon (`icons/aetherium.ico` via
      `build.rs` + `winresource`); previously the .exe had no icon at all.
- [x] Tag-triggered builds: `build.yml` builds all three platforms on `v*`
      tags and publishes GitHub Releases with zipped artifacts (Windows,
      macOS arm64, Linux x86_64 + the requirements-trace report).
- [ ] Build macOS `.dmg` installer in addition to `.app` zip.
- [ ] Build Windows NSIS installer in addition to portable zip.
- [ ] Add optional Windows code signing via repository secrets.
- [ ] Generate release notes automatically from commits.

---

## 7. Shipped after the initial roadmap (2026-09 → 2026-10)

### 7.1 SnakeTail-style log follower — DONE

- [x] `tail -F` (with `-f` fallback) over a dedicated SSH exec channel;
      closing the tab kills the remote tail.
- [x] Toolbar: follow/pause, case-insensitive search with match navigation,
      filter view, bookmarks.
- [x] Configurable regex highlighting (errors/warnings/timestamps/numbers),
      adapted from vscode-logfile-highlighter; rules live in
      `log_highlight.toml` in the config dir.

### 7.2 Remote file transfer extras — DONE

- [x] Drag remote files out to the OS (OLE on Windows, file promise on
      macOS/Linux); drag OS files in to upload; drag tree entries onto
      folders to move them remotely.
- [x] Right-click folder → "Download as ZIP": recursive stored-zip archive
      in ~/Downloads with the standard progress bar (dependency-free zip
      writer in [src/zip.rs](src/zip.rs)).
- [x] File tree: `..` parent row, hidden chevrons on empty folders,
      double-click to connect / remote-edit, per-session connection icons.

### 7.3 Look & feel — DONE

- [x] Bundled Zed theme families (One, Ayu, Gruvbox) parsed from Zed's own
      JSON, persisted selection, header switcher grouped dark/light.
- [x] Lucide-derived icon buttons (Zed's icon set), tooltips, connection
      state shown as colored icons.
- [x] Bundled Lilex monospace font so the terminal grid is always correct.
- [x] Terminal font zoom (cmd/ctrl ±, 0, wheel), persisted in `ui.toml`.
- [x] Heuristic shell syntax coloring for uncolored program output
      (grc-style, `shell_highlight.toml` rules, header toggle).
- [x] Help overlay (header ? button): every shortcut per surface.

### 7.4 REST API & AI integration — DONE

- [x] Self-describing localhost API (`GET /` lists endpoints) with bearer
      token; drives the visible UI's sessions — no hidden connections.
- [x] `POST /logs` opens log-follow tabs; `POST /logs/collect` gathers
      dmesg/journalctl//var/log plus `collect.toml` extras in one JSON
      reply (per-source ok/error, 512 KiB cap). Documented in the repo's
      aetherium skill.
- [x] Requirements tracing (open-very-fast-trace): `docs/requirements/*`,
      trace tags in source, CI gate + HTML report zipped into releases.

### 7.5 Windows parity — DONE

- [x] GUI-subsystem binary (no console window), gpui window icon.
- [x] Auth chain for the user's environments: ssh-agent pipe (incl. the
      "early eof" fallback) → Pageant → default keys → none-auth.
- [x] Windows Terminal clipboard semantics: Ctrl+C copies with selection
      (plain ^C otherwise), Ctrl+V pastes, Ctrl/Shift+Insert.
- [x] Drag-out crash fixed: `DoDragDrop` runs on a helper thread with a
      message-only capture window.

---

## Current status

| Feature | Status | Notes |
|---|---|---|
| SFTP upload/download | `[x]` | Working, progress events emitted |
| Progress bar in status bar | `[x]` | Graphical bar + cancel button |
| Transfer speed/ETA | `[x]` | Rolling 3s window estimate |
| Transfer cancel | `[x]` | Shared flag checked between chunks, partials removed |
| Encrypted password storage | `[x]` | AES-256-GCM, key in `~/.config/aetherium/key` |
| `known_hosts` verification | `[x]` | Trust-on-first-use + reject changed keys |
| Auto-detect default SSH keys | `[x]` | `id_ed25519` → `id_rsa` → `id_ecdsa` |
| Login prompt for missing credentials | `[x]` | Opens profile form instead of connecting blind |
| Key/agent-first auth fallback | `[x]` | Password only sent if agent/default key fail |
| Windows .exe icon | `[x]` | Embedded via `build.rs` + `winresource` |
| Cross-session command history | `[x]` | 200 commands persisted, Shift+↑/↓ recall |
| Remote file edit/watch | `[x]` | Staged temp file, save → sync-back dialog |
| File associations | `[~]` | `$AETHERIUM_EDITOR` + VS Code action; no settings panel yet |
| CLI crate | `[ ]` | REST API covers it for now |
| Release workflow | `[x]` | Tag builds publish GitHub Releases (3 platforms) |
| Log follower (SnakeTail) | `[x]` | tail -F, search/filter/bookmarks, highlighting |
| Themes + icon UI | `[x]` | Zed One/Ayu/Gruvbox, Lucide icons, Lilex font |
| Terminal zoom + shell coloring | `[x]` | cmd/ctrl keys + wheel; grc-style uncolored-cell coloring |
| Folder ZIP download | `[x]` | Stored zip, progress, cancel |
| Help overlay | `[x]` | Shortcuts & features reference |
| REST API + log collection | `[x]` | /logs/collect with collect.toml extras |
| Windows drag-out | `[x]` | OLE on helper thread |

Last updated: 2026-10-10 (log follower, remote edit with sync-back, drag &
drop both ways, folder ZIP download, Zed themes + icon UI, terminal zoom,
shell syntax coloring, help overlay, REST API with `/logs/collect`,
Windows parity work; 78/78 tests green, requirements traceability clean)


