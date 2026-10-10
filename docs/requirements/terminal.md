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
