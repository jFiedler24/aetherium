//! Terminal emulation model: wraps `alacritty_terminal::Term` behind a fair
//! mutex, feeds it bytes from the SSH channel through the vte parser, and
//! translates gpui keystrokes into the byte sequences a PTY expects.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Direction, Line, Point};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::vte::ansi;
use gpui::Keystroke;
use parking_lot::Mutex;

/// Channel used to push user/input bytes towards the SSH session thread.
pub type PtyWriter = tokio::sync::mpsc::UnboundedSender<Vec<u8>>;

/// Async channel the UI registers on the model so the SSH reader thread can
/// wake the repaint loop the moment new output arrives (instead of waiting
/// for the next fixed-interval poll). Unbounded: `send` is synchronous and
/// never blocks the reader thread.
type WakeSender = tokio::sync::mpsc::UnboundedSender<()>;

/// Proxy installed inside the `Term`; it receives terminal events (title
/// changes, bells, OSC replies to write back to the PTY, ...) and marks the
/// UI dirty so the next frame picks the change up.
#[derive(Clone)]
pub struct UiProxy {
    dirty: Arc<AtomicBool>,
    wake: Arc<Mutex<Option<WakeSender>>>,
    pty_writer: Arc<Mutex<Option<PtyWriter>>>,
}

impl UiProxy {
    fn set_dirty(&self) {
        // Only wake when transitioning from clear to set; otherwise the
        // repaint loop would get a wake per feed during a burst.
        if !self.dirty.swap(true, Ordering::AcqRel) {
            if let Some(wake) = self.wake.lock().as_ref() {
                let _ = wake.send(());
            }
        }
    }
}

impl EventListener for UiProxy {
    fn send_event(&self, event: Event) {
        // Answer device-attribute / cursor-position style queries.
        if let Event::PtyWrite(text) = event {
            if let Some(writer) = self.pty_writer.lock().as_ref() {
                let _ = writer.send(text.into_bytes());
            }
        }
        self.set_dirty();
    }
}

/// Shared terminal state: the alacritty grid plus the parser that feeds it.
///
/// The SSH reader thread calls [`TerminalModel::feed`]; the UI thread locks
/// [`TerminalModel::term`] only for the duration of a paint.
/// All clones share the same underlying terminal.
#[derive(Clone)]
pub struct TerminalModel {
    pub term: Arc<FairMutex<Term<UiProxy>>>,
    dirty: Arc<AtomicBool>,
    wake: Arc<Mutex<Option<WakeSender>>>,
    pty_writer: Arc<Mutex<Option<PtyWriter>>>,
    parser: Arc<Mutex<ansi::Processor>>,
    /// Keystrokes echoed into the grid ahead of the server; the matching
    /// server echo is dropped in `feed` so input never appears twice.
    pending_echo: Arc<Mutex<Vec<u8>>>,
    /// Reconstruction of the current input line (bytes sent since the last
    /// Enter), fed from outgoing keystrokes for command-history capture.
    /// Approximate: line editing beyond Backspace/^C/^U is not modeled.
    input_line: Arc<Mutex<Vec<u8>>>,
    /// Count of complete lines ever fed into the grid (counted `\\n`s).
    /// Shared across clones; the SSH/tail reader threads increment it. Log
    /// views use it to give scrollback rows stable anchors: a row captured at
    /// line index `l` with counter `n` keeps the identity `l + n`, because
    /// every newline shifts every existing row's line index by exactly one.
    lines_fed: Arc<AtomicU64>,
}

impl TerminalModel {
    pub fn new(columns: usize, screen_lines: usize) -> Self {
        let dirty = Arc::new(AtomicBool::new(true));
        let wake = Arc::new(Mutex::new(None));
        let pty_writer = Arc::new(Mutex::new(None));
        let proxy = UiProxy {
            dirty: dirty.clone(),
            wake: wake.clone(),
            pty_writer: pty_writer.clone(),
        };
        let size = TermSize::new(columns.max(1), screen_lines.max(1));
        let term = Term::new(Config::default(), &size, proxy);
        Self {
            term: Arc::new(FairMutex::new(term)),
            dirty,
            wake,
            pty_writer,
            parser: Arc::new(Mutex::new(ansi::Processor::new())),
            pending_echo: Arc::new(Mutex::new(Vec::new())),
            input_line: Arc::new(Mutex::new(Vec::new())),
            lines_fed: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Register (or clear) a wake channel that receives a notification every
    /// time the terminal goes from clean to dirty. Called by the UI at tab
    /// creation; `None` disables wakeups.
    pub fn set_wake_channel(&self, wake: Option<WakeSender>) {
        *self.wake.lock() = wake;
    }

    /// Feed raw bytes coming from the SSH channel into the terminal.
    pub fn feed(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // Drop the prefix the server is echoing back of locally echoed
        // keystrokes — those bytes are already on screen.
        let unconfirmed = {
            let mut pending = self.pending_echo.lock();
            let mut skip = 0;
            while skip < bytes.len() && skip < pending.len() && bytes[skip] == pending[skip] {
                skip += 1;
            }
            pending.drain(..skip);
            bytes.len() - skip
        };
        if unconfirmed == 0 {
            return;
        }
        {
            let mut term = self.term.lock();
            self.parser
                .lock()
                .advance(&mut *term, &bytes[bytes.len() - unconfirmed..]);
            let newlines = bytes[bytes.len() - unconfirmed..]
                .iter()
                .filter(|&&b| b == b'\n')
                .count() as u64;
            self.lines_fed.fetch_add(newlines, Ordering::Relaxed);
        }
        self.set_dirty();
    }

    /// Total complete lines ever fed into the grid. See the field doc: the
    /// sum of a row's alacritty line index and this counter at capture time
    /// is a stable identity for that row while it stays in the grid.
    pub fn lines_fed(&self) -> u64 {
        self.lines_fed.load(Ordering::Relaxed)
    }

    /// Valid alacritty line index range: `(min_line0, screen_lines)`. Row
    /// indexes run `-(total_lines - screen_lines) .. screen_lines - 1`.
    pub fn grid_bounds(&self) -> (i32, i32) {
        let term = self.term.lock();
        let screen = term.screen_lines() as i32;
        let history = term.grid().total_lines() as i32 - screen;
        (-history, screen)
    }

    /// Extract one grid row's text. `line0` is alacritty space: it matches
    /// the `point.line` coordinates display_iter reports (0 = top viewport
    /// row, `screen_lines - 1` = bottom, negative = scrollback), so a row
    /// paints at canvas row `line0 + display_offset`. Returns the text with
    /// trailing whitespace trimmed, plus the first column of each remaining
    /// character (for mapping match ranges onto cells). `None` when out of
    /// range.
    pub fn row_text(&self, line0: i32) -> Option<(String, Vec<usize>)> {
        let term = self.term.lock();
        let screen = term.screen_lines() as i32;
        let history = term.grid().total_lines() as i32 - screen;
        if line0 < -history || line0 >= screen {
            return None;
        }
        let row = &term.grid()[Line(line0)];
        let mut text = String::new();
        let mut cols = Vec::new();
        for col in 0..term.columns() {
            let cell = &row[Column(col)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            cols.push(col);
            text.push(cell.c);
        }
        let trimmed = text.trim_end().len();
        text.truncate(trimmed);
        cols.truncate(text.chars().count());
        Some((text, cols))
    }

    /// Whether the server is expected to echo these keystrokes back verbatim:
    /// printable ASCII only. Escape sequences and control keys are consumed
    /// by the remote application, which often doesn't echo them at all.
    fn is_echoable_input(bytes: &[u8]) -> bool {
        !bytes.is_empty() && bytes.iter().all(|byte| (0x20..=0x7e).contains(byte))
    }

    /// Echo printable keystrokes into the grid immediately so typing feels
    /// instant even on high-latency links; the server's identical echo is
    /// deduplicated on arrival in `feed`. Primary screen only — full-screen
    /// applications manage their own display and usually don't echo input.
// [impl->req~local-echo~1]
    pub fn echo_input(&self, bytes: &[u8]) {
        if !Self::is_echoable_input(bytes) {
            return;
        }
        {
            let mut pending = self.pending_echo.lock();
            if pending.len() + bytes.len() > 1024 {
                // No matching echo is arriving (e.g. a password prompt with
                // the remote echo disabled) — stop tracking before this
                // grows without bound.
                pending.clear();
            }
            pending.extend_from_slice(bytes);
            let mut term = self.term.lock();
            self.parser.lock().advance(&mut *term, bytes);
        }
        self.set_dirty();
    }

    /// Set the dirty flag and wake the UI if it transitioned from clear.
    fn set_dirty(&self) {
        if !self.dirty.swap(true, Ordering::AcqRel) {
            if let Some(wake) = self.wake.lock().as_ref() {
                let _ = wake.send(());
            }
        }
    }

    /// Read and clear the dirty flag.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::Acquire)
    }

    /// Wire the channel that carries bytes meant for the PTY (both user
    /// keystrokes and terminal-originated replies such as DA responses).
    pub fn set_pty_writer(&self, writer: PtyWriter) {
        *self.pty_writer.lock() = Some(writer);
    }

    /// Remove the PTY writer (on disconnect).
    pub fn clear_pty_writer(&self) {
        *self.pty_writer.lock() = None;
    }

    /// Mark the UI dirty (e.g. after a display scroll that bypasses the
    /// terminal's own event stream).
    pub fn mark_dirty(&self) {
        self.set_dirty();
    }

    /// Reset to a blank terminal, preserving size and the PTY wiring. Used
    /// when a session connects or disconnects so old output does not persist.
    pub fn reset(&self) {
        {
            let mut term = self.term.lock();
            let size = TermSize::new(term.columns(), term.screen_lines());
            let proxy = UiProxy {
                dirty: self.dirty.clone(),
                wake: self.wake.clone(),
                pty_writer: self.pty_writer.clone(),
            };
            *term = Term::new(Config::default(), &size, proxy);
        }
        // Drop any half-consumed escape sequence from the previous session.
        *self.parser.lock() = ansi::Processor::new();
        self.pending_echo.lock().clear();
        self.input_line.lock().clear();
        self.set_dirty();
    }

    /// Feed bytes that are about to be sent to the PTY into the input-line
    /// tracker. Returns the submitted command when the input contained
    /// Enter.
    pub fn track_input(&self, bytes: &[u8]) -> Option<String> {
        let mut line = self.input_line.lock();
        let mut submitted = None;
        // Skip state for escape sequences: 1 = ESC seen, 2 = inside CSI,
        // 3 = SS3 final byte still to skip.
        let mut escape = 0u8;
        for &byte in bytes {
            if escape == 2 {
                // CSI: parameters/intermediates are skipped until the final
                // byte (0x40..=0x7E).
                if (0x40..=0x7e).contains(&byte) {
                    escape = 0;
                }
                continue;
            }
            if escape == 3 {
                // SS3 final byte.
                escape = 0;
                continue;
            }
            if escape == 1 {
                match byte {
                    b'[' => {
                        escape = 2;
                        continue;
                    }
                    b'O' => {
                        escape = 3;
                        continue;
                    }
                    _ => escape = 0, // Alt-modified character: handle it.
                }
            }
            match byte {
                0x1b => escape = 1,
                b'\r' | b'\n' => {
                    let command = String::from_utf8_lossy(&line).trim().to_string();
                    if !command.is_empty() {
                        submitted = Some(command);
                    }
                    line.clear();
                }
                0x7f | 0x08 => {
                    // Backspace removes the last character (char-aware so
                    // multi-byte input pops whole).
                    let mut text = String::from_utf8_lossy(&line).into_owned();
                    text.pop();
                    *line = text.into_bytes();
                }
                0x03 | 0x15 => {
                    // ^C abandons the line, ^U kills it.
                    line.clear();
                }
                byte if byte >= 0x20 && byte != 0x7f => line.push(byte),
                _ => {}
            }
        }
        submitted
    }

    /// Replace the tracked input line; history recall inserts the recalled
    /// command so the reconstruction matches what the grid will show.
    pub fn set_input_line(&self, line: &str) {
        *self.input_line.lock() = line.as_bytes().to_vec();
    }

    /// Resize the grid; returns true if the size actually changed.
    pub fn resize(&self, columns: usize, screen_lines: usize) -> bool {
        let columns = columns.max(1);
        let screen_lines = screen_lines.max(1);
        let mut term = self.term.lock();
        if term.columns() == columns && term.screen_lines() == screen_lines {
            return false;
        }
        term.resize(TermSize::new(columns, screen_lines));
        drop(term);
        self.set_dirty();
        true
    }

    /// Begin a mouse selection at a grid point. Alacritty's point space:
    /// line 0 is the *bottom* visible row, negative lines are scrollback,
    /// so callers convert a row-from-top with `row - display_offset`.
    /// `side` is which half of the cell the cursor sits in (it decides
    /// whether the boundary cell is included).
    pub fn selection_start(&self, line: i32, col: usize, side: Direction) {
        let mut term = self.term.lock();
        term.selection = Some(Selection::new(
            SelectionType::Simple,
            Point::new(Line(line), Column(col)),
            side,
        ));
        self.set_dirty();
    }

    /// Extend the active selection to a grid point (mouse drag).
    pub fn selection_update(&self, line: i32, col: usize, side: Direction) {
        let mut term = self.term.lock();
        if let Some(selection) = term.selection.as_mut() {
            selection.update(Point::new(Line(line), Column(col)), side);
        }
        self.set_dirty();
    }

    /// End the active selection: a click without a drag selects nothing.
    pub fn selection_end(&self) {
        let mut term = self.term.lock();
        if term.selection.as_ref().is_some_and(|selection| selection.is_empty()) {
            term.selection = None;
        }
        self.set_dirty();
    }

    /// Whether a (possibly still-empty, in-progress) selection exists; used
    /// to decide if a drag should extend it.
    pub fn has_selection(&self) -> bool {
        self.term.lock().selection.is_some()
    }

    /// The selected text, if any: lines joined with newlines, trailing
    /// whitespace on each line trimmed by alacritty.
    pub fn selected_text(&self) -> Option<String> {
        let term = self.term.lock();
        let text = term.selection_to_string()?;
        if text.trim().is_empty() {
            None
        } else {
            Some(text)
        }
    }

    pub fn columns(&self) -> usize {
        self.term.lock().columns()
    }

    pub fn screen_lines(&self) -> usize {
        self.term.lock().screen_lines()
    }

    /// Translate a gpui keystroke into the bytes a PTY (xterm-256color)
    /// expects. `mode` is the current terminal mode (application-cursor keys
    /// etc. change the emitted sequences). Returns `None` for keystrokes that
    /// should not reach the terminal (pure modifier presses, unmapped keys).
    pub fn keystroke_to_bytes(keystroke: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
        let mods = &keystroke.modifiers;
        let key = keystroke.key.as_str();

        // Pure modifier presses produce no bytes.
        if matches!(
            key,
            "shift" | "control" | "alt" | "capslock" | "numlock" | "super" | "fn"
        ) {
            return None;
        }

        // Ctrl+<letter> produces control codes; handle before named keys so
        // that e.g. ctrl-m still works like enter through the generic path.
        // Only when Alt is *not* held: Ctrl+Alt is AltGr on many layouts and
        // must fall through to the plain printable path below.
        if mods.control && !mods.alt {
            // Ctrl+Space is NUL; handle before the named-key arm, which maps
            // "space" to a plain space.
            if key == "space" {
                return Some(vec![0x00]);
            }
            if let Some(bytes) = ctrl_bytes(key) {
                return Some(bytes);
            }
        }

        let app_cursor = mode.contains(TermMode::APP_CURSOR);
        // ESC prefix only for Alt without Ctrl; Ctrl+Alt (AltGr) is printable.
        let esc_prefix = mods.alt && !mods.control;

        let named: Option<&[u8]> = match key {
            "enter" => Some(b"\r"),
            "tab" if mods.shift => Some(b"\x1b[Z"),
            "tab" => Some(b"\t"),
            "backspace" => Some(b"\x7f"),
            "escape" => Some(b"\x1b"),
            "space" => Some(b" "),
            "up" if app_cursor => Some(b"\x1bOA"),
            "up" => Some(b"\x1b[A"),
            "down" if app_cursor => Some(b"\x1bOB"),
            "down" => Some(b"\x1b[B"),
            "right" if app_cursor => Some(b"\x1bOC"),
            "right" => Some(b"\x1b[C"),
            "left" if app_cursor => Some(b"\x1bOD"),
            "left" => Some(b"\x1b[D"),
            "home" if app_cursor => Some(b"\x1bOH"),
            "home" => Some(b"\x1b[H"),
            "end" if app_cursor => Some(b"\x1bOF"),
            "end" => Some(b"\x1b[F"),
            "insert" => Some(b"\x1b[2~"),
            "delete" => Some(b"\x1b[3~"),
            "pageup" => Some(b"\x1b[5~"),
            "pagedown" => Some(b"\x1b[6~"),
            "f1" => Some(b"\x1bOP"),
            "f2" => Some(b"\x1bOQ"),
            "f3" => Some(b"\x1bOR"),
            "f4" => Some(b"\x1bOS"),
            "f5" => Some(b"\x1b[15~"),
            "f6" => Some(b"\x1b[17~"),
            "f7" => Some(b"\x1b[18~"),
            "f8" => Some(b"\x1b[19~"),
            "f9" => Some(b"\x1b[20~"),
            "f10" => Some(b"\x1b[21~"),
            "f11" => Some(b"\x1b[23~"),
            "f12" => Some(b"\x1b[24~"),
            _ => None,
        };
        if let Some(bytes) = named {
            let mut out = Vec::new();
            if esc_prefix {
                out.push(0x1b);
            }
            out.extend_from_slice(bytes);
            return Some(out);
        }

        // Printable text via key_char (layout-aware, shift already applied).
        if let Some(key_char) = keystroke.key_char.as_deref() {
            if key_char.is_empty() || mods.platform {
                return None;
            }
            let mut out = Vec::new();
            if esc_prefix {
                out.push(0x1b);
            }
            out.extend_from_slice(key_char.as_bytes());
            return Some(out);
        }

        None
    }
}

/// Map ctrl-modified keys to their control bytes.
fn ctrl_bytes(key: &str) -> Option<Vec<u8>> {
    if key.chars().count() != 1 {
        return None;
    }
    let ch = key.chars().next()?;
    let byte = match ch {
        'a'..='z' => ch as u8 - b'a' + 1,
        '2' | '@' | ' ' => 0x00,
        '3' | '[' => 0x1b,
        '4' | '\\' => 0x1c,
        '5' | ']' => 0x1d,
        '6' | '^' => 0x1e,
        '7' | '-' | '_' => 0x1f,
        '8' | '?' => 0x7f,
        _ => return None,
    };
    Some(vec![byte])
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Modifiers;

    fn keystroke(key: &str, key_char: Option<&str>, mods: Modifiers) -> Keystroke {
        Keystroke {
            modifiers: mods,
            key: key.to_string(),
            key_char: key_char.map(str::to_string),
        }
    }

    fn plain(key: &str, key_char: Option<&str>) -> Keystroke {
        keystroke(key, key_char, Modifiers::default())
    }

    fn to_bytes(ks: &Keystroke) -> Option<Vec<u8>> {
        TerminalModel::keystroke_to_bytes(ks, TermMode::empty())
    }

    #[test]
    fn printable_characters() {
        assert_eq!(
            to_bytes(&plain("a", Some("a"))),
            Some(b"a".to_vec())
        );
        assert_eq!(
            to_bytes(&keystroke(
                "a",
                Some("A"),
                Modifiers {
                    shift: true,
                    ..Default::default()
                }
            )),
            Some(b"A".to_vec())
        );
        assert_eq!(
            to_bytes(&plain("space", Some(" "))),
            Some(b" ".to_vec())
        );
        // Non-ASCII input goes through key_char.
        assert_eq!(
            to_bytes(&plain("é", Some("é"))),
            Some("é".as_bytes().to_vec())
        );
    }

    /// Extract a screen line's text the way the renderer does.
    fn visible_line(model: &TerminalModel, line: i32) -> String {
        let term = model.term.lock();
        let mut text = String::new();
        for indexed in term.renderable_content().display_iter {
            if indexed.point.line.0 == line {
                text.push(indexed.cell.c);
            }
        }
        text.trim_end().to_string()
    }

    #[test]
    fn local_echo_dedupes_server_echo() {
        let model = TerminalModel::new(80, 24);
        // The server's identical echo is dropped: the text shows once.
        model.echo_input(b"abc");
        model.feed(b"abc");
        assert_eq!(visible_line(&model, 0), "abc");
        // A partial echo match strips only what arrived; the rest stays
        // pending and is not printed again.
        model.echo_input(b"xy");
        model.feed(b"x");
        assert_eq!(visible_line(&model, 0), "abcxy");
        // Data that is not an echo at all renders as-is.
        model.feed(b"Z>");
        assert_eq!(visible_line(&model, 0), "abcxyZ>");
        // Control sequences are never locally echoed.
        model.echo_input(b"\x1b[A");
        assert_eq!(model.pending_echo.lock().len(), 1);
    }

    #[test]
    fn named_keys() {
        assert_eq!(
            to_bytes(&plain("enter", Some("\n"))),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            to_bytes(&plain("backspace", None)),
            Some(b"\x7f".to_vec())
        );
        assert_eq!(
            to_bytes(&plain("tab", Some("\t"))),
            Some(b"\t".to_vec())
        );
        assert_eq!(
            to_bytes(&plain("escape", None)),
            Some(b"\x1b".to_vec())
        );
        assert_eq!(
            to_bytes(&plain("delete", None)),
            Some(b"\x1b[3~".to_vec())
        );
    }

    #[test]
    fn arrow_keys() {
        assert_eq!(
            to_bytes(&plain("up", None)),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            to_bytes(&plain("down", None)),
            Some(b"\x1b[B".to_vec())
        );
        assert_eq!(
            to_bytes(&plain("right", None)),
            Some(b"\x1b[C".to_vec())
        );
        assert_eq!(
            to_bytes(&plain("left", None)),
            Some(b"\x1b[D".to_vec())
        );
    }

    #[test]
    fn ctrl_letters() {
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        assert_eq!(
            to_bytes(&keystroke("a", None, ctrl)),
            Some(vec![0x01])
        );
        assert_eq!(
            to_bytes(&keystroke("c", None, ctrl)),
            Some(vec![0x03])
        );
        assert_eq!(
            to_bytes(&keystroke("d", None, ctrl)),
            Some(vec![0x04])
        );
        assert_eq!(
            to_bytes(&keystroke("l", None, ctrl)),
            Some(vec![0x0c])
        );
        assert_eq!(
            to_bytes(&keystroke("z", None, ctrl)),
            Some(vec![0x1a])
        );
    }

    #[test]
    fn alt_prefixes_escape() {
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        assert_eq!(
            to_bytes(&keystroke("b", Some("b"), alt)),
            Some(b"\x1bb".to_vec())
        );
        assert_eq!(
            to_bytes(&keystroke("left", None, alt)),
            Some(b"\x1b\x1b[D".to_vec())
        );
    }

    #[test]
    fn altgr_is_printable() {
        // Ctrl+Alt (AltGr on many layouts) sends the printable character
        // without a control code or ESC prefix.
        let altgr = Modifiers {
            control: true,
            alt: true,
            ..Default::default()
        };
        assert_eq!(
            to_bytes(&keystroke("e", Some("€"), altgr)),
            Some("€".as_bytes().to_vec())
        );
        assert_eq!(
            to_bytes(&keystroke("q", Some("@"), altgr)),
            Some(b"@".to_vec())
        );
    }

    #[test]
    fn ctrl_space_is_nul() {
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        assert_eq!(
            to_bytes(&keystroke("space", Some(" "), ctrl)),
            Some(vec![0x00])
        );
    }

    #[test]
    fn shift_tab_is_backtab() {
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            to_bytes(&keystroke("tab", Some("\t"), shift)),
            Some(b"\x1b[Z".to_vec())
        );
    }

    #[test]
    fn application_cursor_mode() {
        let app = TermMode::APP_CURSOR;
        let arrow = |key: &str| {
            TerminalModel::keystroke_to_bytes(&plain(key, None), app)
        };
        assert_eq!(arrow("up"), Some(b"\x1bOA".to_vec()));
        assert_eq!(arrow("down"), Some(b"\x1bOB".to_vec()));
        assert_eq!(arrow("right"), Some(b"\x1bOC".to_vec()));
        assert_eq!(arrow("left"), Some(b"\x1bOD".to_vec()));
        assert_eq!(arrow("home"), Some(b"\x1bOH".to_vec()));
        assert_eq!(arrow("end"), Some(b"\x1bOF".to_vec()));
        // Unrelated keys are unaffected by application-cursor mode.
        assert_eq!(arrow("delete"), Some(b"\x1b[3~".to_vec()));
    }

    #[test]
    fn unmapped_and_modifiers() {
        assert_eq!(
            to_bytes(&plain("shift", None)),
            None
        );
        assert_eq!(
            to_bytes(&plain("control", None)),
            None
        );
        let platform = Modifiers {
            platform: true,
            ..Default::default()
        };
        assert_eq!(
            to_bytes(&keystroke("v", Some("v"), platform)),
            None
        );
    }

    #[test]
    fn feed_updates_grid_and_dirty_flag() {
        let model = TerminalModel::new(80, 24);
        model.feed(b"hello\r\nworld");
        let term = model.term.lock();
        let grid = term.grid();
        let row0: String = (0..5).map(|i| grid[alacritty_terminal::index::Line(0)][alacritty_terminal::index::Column(i)].c).collect();
        assert_eq!(row0, "hello");
        let row1: String = (0..5).map(|i| grid[alacritty_terminal::index::Line(1)][alacritty_terminal::index::Column(i)].c).collect();
        assert_eq!(row1, "world");
        drop(term);
        assert!(model.take_dirty());
        assert!(!model.take_dirty());
    }

    #[test]
    fn resize_changes_dimensions() {
        let model = TerminalModel::new(80, 24);
        assert!(model.resize(120, 40));
        assert!(!model.resize(120, 40));
        assert_eq!(model.columns(), 120);
        assert_eq!(model.screen_lines(), 40);
        // Zero sizes are clamped.
        assert!(model.resize(0, 0));
        assert_eq!(model.columns(), 1);
        assert_eq!(model.screen_lines(), 1);
    }

    #[test]
    fn selection_extracts_multiline_text() {
        let model = TerminalModel::new(80, 24);
        model.feed(b"first line\r\nsecond line\r\nthird line");
        // Rows from top: 0="first line", 1="second line", 2="third line".
        // Unscrolled, grid line == row from top (bottom-origin space is
        // shifted by -display_offset only when the view is scrolled).
        model.selection_start(0, 0, Direction::Left);
        model.selection_update(1, 5, Direction::Left);
        let text = model.selected_text().expect("selection extracts");
        assert!(text.contains("first line"), "got {text:?}");
        // Ends where it was dragged: the second line is cut at column 5.
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "first line");
        assert_eq!(lines[1], "secon", "got {text:?}");
        // A head sitting in the right half of the end cell includes it.
        model.selection_update(1, 5, Direction::Right);
        let text = model.selected_text().expect("selection extracts");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[1], "second", "got {text:?}");
        // Dragging above the anchor crosses it: the range normalizes and the
        // selection expands upward from the anchor instead of shrinking. The
        // head's side cuts the boundary: Right half of the 'r' cell leaves
        // 'r' out (cut after the cell), Left half includes it.
        model.selection_start(1, 5, Direction::Left);
        model.selection_update(0, 2, Direction::Right);
        let text = model.selected_text().expect("selection extracts");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "st line", "got {text:?}");
        assert_eq!(lines[1], "secon", "got {text:?}");
        model.selection_update(0, 2, Direction::Left);
        let text = model.selected_text().expect("selection extracts");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "rst line", "got {text:?}");
    }

    #[test]
    fn selection_survives_scrolling_the_view() {
        use alacritty_terminal::grid::Scroll;
        let model = TerminalModel::new(10, 4);
        model.feed(b"aaa\r\nbbb\r\nccc\r\nddd\r\neee\r\nfff");
        // Row 2 from the top is "eee" (visible rows: ccc ddd eee fff).
        model.selection_start(2, 0, Direction::Left);
        model.selection_update(2, 2, Direction::Right);
        assert_eq!(model.selected_text().as_deref(), Some("eee"));
        // Scroll the view up: the selection must stay anchored to the same
        // content (alacritty rotates it), not stick to the viewport row.
        model.term.lock().scroll_display(Scroll::Delta(2));
        assert_eq!(model.selected_text().as_deref(), Some("eee"));
        // And after scrolling back down it is still the same text.
        model.term.lock().scroll_display(Scroll::Delta(-2));
        assert_eq!(model.selected_text().as_deref(), Some("eee"));
    }

    #[test]
    fn click_without_drag_selects_nothing() {
        let model = TerminalModel::new(80, 24);
        model.feed(b"hello");
        model.selection_start(0, 1, Direction::Left);
        assert!(model.has_selection());
        model.selection_end();
        assert!(!model.has_selection());
        assert_eq!(model.selected_text(), None);
    }

    #[test]
    fn whitespace_only_selection_copies_nothing() {
        let model = TerminalModel::new(80, 24);
        model.feed(b"ab");
        model.selection_start(0, 5, Direction::Left);
        model.selection_update(0, 9, Direction::Left);
        assert_eq!(model.selected_text(), None);
    }

    #[test]
    fn input_tracker_captures_submitted_commands() {
        let model = TerminalModel::new(80, 24);
        // Nothing submitted yet.
        assert_eq!(model.track_input(b"ls -la"), None);
        // Enter completes the command and clears the line.
        assert_eq!(model.track_input(b"\r"), Some("ls -la".to_string()));
        // The next command starts fresh.
        assert_eq!(model.track_input(b"pwd\r"), Some("pwd".to_string()));
        // Empty lines submit nothing.
        assert_eq!(model.track_input(b"\r"), None);
    }

    #[test]
    fn input_tracker_models_basic_line_editing() {
        let model = TerminalModel::new(80, 24);
        // Backspace deletes the previous character.
        model.track_input(b"abc");
        model.track_input(b"\x7f");
        assert_eq!(model.track_input(b"d\r"), Some("abd".to_string()));
        // Multi-byte characters pop whole.
        model.track_input("é".as_bytes());
        model.track_input(b"\x7f");
        assert_eq!(model.track_input(b"x\r"), Some("x".to_string()));
        // ^C and ^U abandon the line.
        model.track_input(b"garbage");
        model.track_input(b"\x03");
        assert_eq!(model.track_input(b"ok\r"), Some("ok".to_string()));
        model.track_input(b"nope");
        model.track_input(b"\x15");
        assert_eq!(model.track_input(b"fine\r"), Some("fine".to_string()));
        // Escape sequences and tabs do not join the line.
        model.track_input(b"\x1b[A\x1b[B\t");
        assert_eq!(model.track_input(b"clean\r"), Some("clean".to_string()));
    }

    #[test]
    // [utest->req~piped-newline-handling~1]
    fn log_grids_start_lines_at_column_zero() {
        // Piped remote output is \\n-terminated (no PTY translating to
        // \\r\\n) and alacritty routes raw LF to `linefeed` without LNM, so
        // the tail reader translates lone \\n to \\r\\n before feeding. Feed
        // the translated form here; every line must start at column 0.
        let model = TerminalModel::new(80, 10);
        for i in 0..30 {
            model.feed(format!("line{i:02}\r\n").as_bytes());
        }
        let term = model.term.lock();
        let history = term.grid().total_lines() - term.grid().screen_lines();
        // "line00" survives in scrollback; "line29" is the last content row,
        // right above the empty cursor line at the top line index.
        let (oldest, _) = model_row(&term, -(history as i32));
        assert!(oldest.starts_with("line00"), "got {oldest:?}");
        let (newest, _) = model_row(&term, term.screen_lines() as i32 - 2);
        assert!(newest.starts_with("line29"), "got {newest:?}");
    }

    #[test]
    fn line_anchor_is_stable_across_feeds() {
        // A row's identity = line index + lines_fed at capture time; it must
        // resolve to the same text after more lines arrive.
        let model = TerminalModel::new(80, 10);
        for i in 0..15 {
            model.feed(format!("line{i:02}\r\n").as_bytes());
        }
        let term = model.term.lock();
        let fed = model.lines_fed();
        // Find "line03" wherever it currently is.
        let history = (term.grid().total_lines() - term.grid().screen_lines()) as i32;
        let mut anchor = None;
        for l in -history..term.screen_lines() as i32 {
            let (text, _) = model_row(&term, l);
            if text.starts_with("line03") {
                anchor = Some(l as i64 + fed as i64);
                break;
            }
        }
        drop(term);
        let anchor = anchor.expect("line03 in grid");
        for i in 15..40 {
            model.feed(format!("line{i:02}\r\n").as_bytes());
        }
        let term = model.term.lock();
        let line0 = (anchor - model.lines_fed() as i64) as i32;
        let (text, _) = model_row(&term, line0);
        assert!(text.starts_with("line03"), "got {text:?} at {line0}");
    }

    /// Helper: row text via the same API the log view uses.
    fn model_row(term: &Term<UiProxy>, line0: i32) -> (String, Vec<usize>) {
        let row = &term.grid()[Line(line0)];
        let mut text = String::new();
        let mut cols = Vec::new();
        for col in 0..term.columns() {
            let cell = &row[Column(col)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            cols.push(col);
            text.push(cell.c);
        }
        let trimmed = text.trim_end().len();
        text.truncate(trimmed);
        cols.truncate(text.chars().count());
        (text, cols)
    }
}
