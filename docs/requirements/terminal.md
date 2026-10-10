# Terminal

## feat~terminal-emulation~1

The app shall render an interactive terminal over SSH (xterm-256color)
with correct glyphs, colors, selection, copy, scrollback, and font
zoom, matching the rendering quality of Zed's terminal.

**Covers:** creq~ssh-terminal-for-homelab~1

**Needs:** req, impl

## req~bundled-monospace-font~1

A true monospace font (Lilex) shall be bundled so the terminal cell
grid is always correct even when system font enumeration fails to
expose a monospace to the UI toolkit.

**Covers:** feat~terminal-emulation~1

**Needs:** impl

## req~local-echo~1

Printable keystrokes shall be echoed into the grid instantly and the
server's identical echo deduplicated, so typing stays responsive on
high-latency links.

**Covers:** feat~terminal-emulation~1

**Needs:** impl

## feat~theme-system~1

The whole UI — including terminal colors — shall be theme-driven, with
Zed's standard theme families bundled, persisted selection, and a
header switcher grouped dark/light.

**Covers:** creq~slick-modern-ui~1

**Needs:** req, impl

## req~bundled-zed-themes~1

Themes shall be parsed from the original Zed theme JSON (One, Ayu,
Gruvbox families) rather than hardcoded palettes.

**Covers:** feat~theme-system~1

**Needs:** impl

## feat~icon-ui~1

Header actions and status indicators shall use icons (from Zed's
Lucide-derived set) with hover tooltips instead of text labels where an
icon is unambiguous; connection state shall be shown as a colored icon.

**Covers:** creq~slick-modern-ui~1

**Needs:** impl

## feat~terminal-font-zoom~1

The terminal text size shall be increaseable and decreaseable at runtime,
via keyboard shortcuts and the mouse wheel, with a one-key reset to the
default; the chosen size persists across launches.

**Covers:** creq~zoom-terminal-text~1

**Needs:** req, impl

## req~font-zoom-shortcuts~1

Zoom is bound to the platform modifier (cmd +/- / cmd 0, cmd+wheel) on
macOS; on Windows and Linux, where that modifier is the Windows key, the
same actions are additionally bound to ctrl.

**Covers:** feat~terminal-font-zoom~1

**Needs:** impl

## feat~shell-syntax-coloring~1

Shell tabs shall get heuristic syntax coloring for output the remote
program left uncolored, toggled from the header and configurable via
regex rules in the config directory.

**Covers:** creq~colorize-uncolored-terminal~1

**Needs:** req, impl

## req~uncolored-cell-coloring~1

Highlight rules may recolor only cells still carrying the terminal's
default foreground/background: any SGR color the program set (bright
text, backgrounds, inverse video) stays untouched, so `ls --color`, vim,
htop and friends render exactly as intended.

**Covers:** feat~shell-syntax-coloring~1

**Needs:** impl, utest

## feat~help-overlay~1

A header button shall open an in-app help overlay listing the app's
shortcuts and features per surface (terminal, file tree, log tabs,
general), dismissible with Escape or a click outside.

**Covers:** creq~in-app-help~1

**Needs:** req, impl

## req~help-shortcut-list~1

The help overlay's shortcut list shall stay in sync with the real
bindings (zoom keys, copy/paste, history recall, log-tab keys) and cover
the file-tree actions including the folder ZIP download.

**Covers:** feat~help-overlay~1

**Needs:** impl, utest
