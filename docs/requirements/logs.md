# Log Following

## feat~log-follow-view~1

The app shall open read-only tabs that follow a remote log file in
real time (streaming the tail over the SSH connection), openable from
the file tree's context menu.

**Covers:** creq~watch-remote-logs-like-snaketail~1

**Needs:** req, impl

## req~log-rotation~1

The follow command shall use `tail -F` (by filename) with an automatic
fallback to `-f`, so log rotation does not freeze the view.

**Covers:** feat~log-follow-view~1

**Needs:** impl

## req~log-highlighting~1

Log lines shall be syntax-highlighted via configurable regex rules
(errors bold red, warnings amber, timestamps cyan, …), editable in the
config directory.

**Covers:** feat~log-follow-view~1

**Needs:** impl, utest

## feat~snaketail-tools~1

Log tabs shall offer SnakeTail's working tools: pause/resume follow,
case-insensitive search with match navigation, a filter view showing
only matching lines, and bookmarks on lines.

**Covers:** creq~watch-remote-logs-like-snaketail~1

**Needs:** impl

## req~piped-newline-handling~1

Channel output without a PTY shall be translated LF → CRLF before
reaching the grid, because the terminal emulator keeps the column on
bare line feeds.

**Covers:** feat~log-follow-view~1

**Needs:** impl, utest
