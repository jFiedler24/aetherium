# aetherium

A SSH/SFTP client built with **gpui** — the GPU-accelerated UI
framework extracted from the [Zed editor](https://zed.dev). Same rendering stack,
same snappy text layout, same dark aesthetic.

[![Build](https://github.com/jFiedler24/aetherium/actions/workflows/build.yml/badge.svg)](https://github.com/jFiedler24/aetherium/actions/workflows/build.yml)

## Features

- **Multiple tabs** — every connection opens in its own tab with an independent
  SSH session, terminal, and file tree; switch or close tabs from the tab bar.
- **Sidebar tabs** — the left sidebar switches between **Sessions** (profiles +
  recents), **Files** (the remote file browser, full height), and **Logs** (the
  tool's own log). Drag the splitter between sidebar and terminal to resize.
- **Remote file tree** (Files tab) — browses the SSH host over SFTP with lazy
  directory expansion, dirs-first sorting and Zed-style drag & drop: drag entries
  onto directories (or the background) to move them remotely via SFTP rename,
  with drop-target highlighting and auto-expanding folders; dragging OS files in
  uploads them. The **≡** button toggles a details column with file sizes.
- **Tool log** (Logs tab) — every connection event and failure collected in one
  place with timestamps: connect/disconnect, authentication and SFTP errors
  (e.g. permission denied), upload/download/rename results, tail failures.
  Keeps the last 1000 entries; auto-scrolls to the newest.
- **Remote terminal** (right pane) — real PTY shell (`xterm-256color`) rendered on a
  GPU canvas via `alacritty_terminal` grid + gpui text shaping. Full color support
  (16/256/truecolor), bold/dim/inverse/hidden attributes, block/beam/underline cursor.
- **Log follower** — right-click any remote file in the tree → **tail -f** opens a
  read-only tab that follows the file over a dedicated SSH exec channel (closing the
  tab kills the remote tail). Log tabs get regex-based highlighting: ERROR/FATAL red,
  WARN yellow, INFO blue, DEBUG/TRACE dim, plus dates and numbers.
- **Configurable log highlighting** — rules live in
  `~/.config/aetherium/log_highlight.toml` (`[[rule]] pattern/color/bold`); the
  embedded default is adapted from the MIT-licensed
  [vscode-logfile-highlighter](https://github.com/emilast/LogFileHighlighter).
- **Profile manager** — saved connections (name, host, port, user, auth) persisted as
  TOML at `~/.config/aetherium/profiles.toml`. Add / edit / delete / connect from the UI.
- **Recent sessions** — last 10 connections (`user@host`, relative time) in the sidebar,
  one click to reconnect; stored at `~/.config/aetherium/recents.toml`.
- **Drag & drop file transfer** — drop local files/folders from your OS file manager onto
  the remote tree (or a directory row) to upload recursively via SFTP; drop onto the
  terminal to upload to the remote home dir. Select a remote file/dir and hit
  **⇩** to save it to `~/Downloads` (collision-safe). Live progress in the
  status bar. Uses gpui's cross-platform drop API, so the same code works on
  X11, macOS and Windows backends.
- **Remote file moves** — drag a tree entry onto a directory row to move it there;
  dropping on a file row or the tree background moves it into that directory or the
  tree root. Moves run as SFTP renames; both affected directories refresh
  afterwards.
- **Auth methods** — password, key file (with optional passphrase), ssh-agent.
- Zed's default dark theme and Zed Sans/Mono typography, ~60fps terminal
  repaints only when dirty.

## Theme & fonts

The UI colors are transcribed 1:1 from Zed's built-in default dark theme and
its `terminal_ansi_*` palette (Zed is MIT-licensed; the values live in
`src/main.rs` / `src/ui.rs` next to the code that uses them).

Fonts are referenced by name — **Zed Sans** (UI) and **Zed Mono** (terminal) —
and fall back to system fonts when not installed. Zed's fonts are licensed
under the SIL Open Font License 1.1, which permits this use. They ship inside
Zed itself, so for the exact Zed look copy the font files from a Zed
installation (e.g. `Zed.app/Contents/Resources` on macOS) or install them
system-wide. aetherium also loads every `.ttf`/`.otf`/`.ttc` found in
`~/.config/aetherium/fonts/` on startup, so dropping the files there works too.

## Tech stack

| piece | crate |
|---|---|
| UI framework | `gpui 0.2.2` (from Zed, x11 backend) |
| SSH | `russh 0.63` (pure Rust) |
| SFTP | `russh-sftp 3.0` |
| Terminal emulation | `alacritty_terminal 0.26` |
| Async runtime | `tokio` (dedicated background thread) |

Architecture: a dedicated tokio thread owns all SSH I/O. The UI thread never blocks —
commands go out over a tokio mpsc, events/listings come back over std mpsc, and the
terminal grid lives behind a `FairMutex` with an atomic dirty flag polled at 16 ms.

## Build & run

Requirements: Rust ≥ 1.85 (stable). Each platform has a build script in
`scripts/` that produces a release binary and a packaged artifact in `dist/`.
The same scripts run in CI (`.github/workflows/build.yml`) on every push to
`main`; pushing a `v*` tag additionally creates a GitHub release with all three
packages attached.

### Linux (Debian/Ubuntu)

```bash
sudo apt install libxkbcommon-dev libxkbcommon-x11-dev libfontconfig-dev
./scripts/build-linux.sh       # → dist/aetherium-<ver>-linux-<arch>.tar.gz
```

No Wayland dev packages needed — aetherium builds gpui with only the `x11`
feature (Linux deps are target-gated inside gpui, so the same manifest builds
unchanged on macOS and Windows).

### macOS

```bash
./scripts/build-macos.sh       # → aetherium.app + dist/aetherium-<ver>-macos-<arch>.zip
open aetherium.app
```

Packages a proper `.app` bundle with the aetherium Dock icon (built from
`aetherium_icons_dark_v2/main_executable.svg`) and ad-hoc code signs it.

### Windows

```powershell
.\scripts\build-windows.ps1    # → dist\aetherium-<ver>-windows-x86_64.zip
```

For quick development on any OS: `cargo run` (debug) / `cargo test`.

> **Note for sandboxed/FUSE mounts:** if you build under a noexec mount
> (like `/mnt/agents`), point cargo's target dir elsewhere:
> `CARGO_TARGET_DIR=$HOME/.aetherium-target cargo run`

## Usage

1. Click **+ New** in the header → fill in name, host, port, username, auth → **Save**.
2. Select the profile in the sidebar → **Connect**.
3. Terminal opens on the right; the file tree populates from the remote home directory.
   Click a directory to expand it lazily.
4. Clipboard paste into the terminal works (Ctrl/Cmd-V); window resize propagates PTY
   size changes to the remote shell.

Profiles file format (`~/.config/aetherium/profiles.toml`):

```toml
[[profiles]]
name = "dev box"
host = "192.168.1.10"
port = 22
username = "me"

[profiles.auth]
type = "password"
password = "hunter2"          # or: type = "key_file", path = "~/.ssh/id_ed25519", passphrase = "..."
                              # or: type = "agent"
```

## v1 limitations (by design)

- Accepts any host key (logged as a warning) — no known-hosts verification UI yet.
- Tabs are per-window; no splits within a tab.
- Log highlighting rules are read when a log tab opens (restart the tab to reload).
- Runtime OSC palette changes aren't honored (static Alacritty default palette).
- Dragging files *out* of the app to the OS (download-via-drag) isn't supported —
  use the Download button instead.
- gpui upstream is Linux/macOS-first; Windows support is preliminary (the app's
  drag & drop and UI code use only cross-platform gpui APIs, so a Windows build is
  blocked only on gpui's backend maturity, not on app code).

## Development

```bash
cargo check -j2   # fast iteration
cargo test        # profiles round-trip + keystroke→bytes mapping tests
```

Module map: `profiles.rs` (persistence) · `recents.rs` (recent sessions) ·
`session.rs` (russh backend thread: shell PTY, SFTP, transfers, tail channels) ·
`terminal_model.rs` (alacritty grid + vte feed) · `log_highlight.rs` (regex log
highlighting rules) · `text_field.rs` (IME-aware input) ·
`ui.rs` (RootView: sidebar, tree, tab bar, terminal canvas, context menu) ·
`main.rs` (entry + theme).

Releases: tag a version and push — CI builds all three platforms and attaches
the packages to a GitHub release:

```bash
git tag v0.1.0 && git push origin v0.1.0
```
