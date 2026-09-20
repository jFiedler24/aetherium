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
- [ ] Format byte counts as KB/MB/GB — done via existing `format_size`.
- [ ] Render a graphical progress bar with percentage (currently text-only).
- [ ] Support multiple concurrent transfers (queue + per-transfer rows).
- [ ] Add a "Cancel transfer" action.

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

### 3.2 Cross-session command history

- [ ] Persist last 200 terminal commands to `~/.config/aetherium/command_history.toml`.
- [ ] Deduplicate consecutive identical commands.
- [ ] Bind configurable hotkeys (`Shift+↑` / `Shift+↓`) to recall history in the terminal.

---

## 4. Remote file editing

### 4.1 Download → open → watch → upload workflow

- [ ] Download a remote file to a local temp cache.
- [ ] Open it with the OS default application or a user-configured tool.
- [ ] Watch the local file for changes and upload it back automatically.

### 4.2 File association / tool mapping

- [ ] Add settings for mapping extensions (e.g., `.txt`, `.conf`) to applications.
- [ ] Build a simple gpui settings panel to manage associations.
- [ ] Use associations when opening remote files.

---

## 5. CLI companion

- [ ] Add a new `crates/aetherium-cli/` workspace member.
- [ ] Commands (in priority order):
  - [ ] `profiles` — list saved profiles as JSON.
  - [ ] `export-ssh-config` — generate an `~/.ssh/config` snippet from profiles.
  - [ ] `connect <profile>` — open a terminal session via the existing backend.
  - [ ] `exec <profile> <command>` — run a remote command.
  - [ ] `cp <profile:path> <local>` / `cp <local> <profile:path>` — file transfers.

---

## 6. Distribution & CI

- [ ] Create a separate `release.yml` triggered on `v*` tags.
- [ ] Build macOS `.dmg` installer in addition to `.app` zip.
- [ ] Build Windows NSIS installer in addition to portable zip.
- [ ] Add optional Windows code signing via repository secrets.
- [ ] Generate release notes automatically from commits.

---

## Current status

| Feature | Status | Notes |
|---|---|---|
| SFTP upload/download | `[x]` | Working, progress events emitted |
| Progress bar in status bar | `[x]` | Text-based; graphical bar still open |
| Transfer speed/ETA | `[x]` | Rolling 3s window estimate |
| Encrypted password storage | `[x]` | AES-256-GCM, key in `~/.config/aetherium/key` |
| `known_hosts` verification | `[x]` | Trust-on-first-use + reject changed keys |
| Auto-detect default SSH keys | `[x]` | `id_ed25519` → `id_rsa` → `id_ecdsa` |
| Cross-session command history | `[ ]` | Needs terminal input hook |
| Remote file edit/watch | `[ ]` | Larger backend + UI task |
| File associations | `[ ]` | Depends on remote-edit |
| CLI crate | `[ ]` | Separate crate |
| Release workflow | `[ ]` | CI-only |

Last updated: 2026-09-20 (encrypted passwords, known_hosts verification, key
auto-detect, and transfer speed/ETA landed and pass `cargo test --locked`,
27/27 tests green)

