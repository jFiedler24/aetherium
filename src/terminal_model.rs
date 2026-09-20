//! Terminal emulation model: wraps `alacritty_terminal::Term` behind a fair
//! mutex, feeds it bytes from the SSH channel through the vte parser, and
//! translates gpui keystrokes into the byte sequences a PTY expects.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::vte::ansi;
use gpui::Keystroke;
use parking_lot::Mutex;

/// Channel used to push user/input bytes towards the SSH session thread.
pub type PtyWriter = tokio::sync::mpsc::UnboundedSender<Vec<u8>>;

/// Proxy installed inside the `Term`; it receives terminal events (title
/// changes, bells, OSC replies to write back to the PTY, ...) and marks the
/// UI dirty so the next frame picks the change up.
#[derive(Clone)]
pub struct UiProxy {
    dirty: Arc<AtomicBool>,
    pty_writer: Arc<Mutex<Option<PtyWriter>>>,
}

impl EventListener for UiProxy {
    fn send_event(&self, event: Event) {
        // Answer device-attribute / cursor-position style queries.
        if let Event::PtyWrite(text) = event {
            if let Some(writer) = self.pty_writer.lock().as_ref() {
                let _ = writer.send(text.into_bytes());
            }
        }
        self.dirty.store(true, Ordering::Release);
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
    pty_writer: Arc<Mutex<Option<PtyWriter>>>,
    parser: Arc<Mutex<ansi::Processor>>,
}

impl TerminalModel {
    pub fn new(columns: usize, screen_lines: usize) -> Self {
        let dirty = Arc::new(AtomicBool::new(true));
        let pty_writer = Arc::new(Mutex::new(None));
        let proxy = UiProxy {
            dirty: dirty.clone(),
            pty_writer: pty_writer.clone(),
        };
        let size = TermSize::new(columns.max(1), screen_lines.max(1));
        let term = Term::new(Config::default(), &size, proxy);
        Self {
            term: Arc::new(FairMutex::new(term)),
            dirty,
            pty_writer,
            parser: Arc::new(Mutex::new(ansi::Processor::new())),
        }
    }

    /// Feed raw bytes coming from the SSH channel into the terminal.
    pub fn feed(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        {
            let mut term = self.term.lock();
            self.parser.lock().advance(&mut *term, bytes);
        }
        self.dirty.store(true, Ordering::Release);
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
        self.dirty.store(true, Ordering::Release);
    }

    /// Reset to a blank terminal, preserving size and the PTY wiring. Used
    /// when a session connects or disconnects so old output does not persist.
    pub fn reset(&self) {
        {
            let mut term = self.term.lock();
            let size = TermSize::new(term.columns(), term.screen_lines());
            let proxy = UiProxy {
                dirty: self.dirty.clone(),
                pty_writer: self.pty_writer.clone(),
            };
            *term = Term::new(Config::default(), &size, proxy);
        }
        // Drop any half-consumed escape sequence from the previous session.
        *self.parser.lock() = ansi::Processor::new();
        self.dirty.store(true, Ordering::Release);
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
        self.dirty.store(true, Ordering::Release);
        true
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
}
