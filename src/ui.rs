//! Root view: Zed-style layout with a header bar, a sidebar (profiles +
//! remote file tree), the terminal canvas, and a status bar.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::Direction;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};
use gpui::{
    App, Bounds, BorderStyle, ClipboardItem, Context, CursorStyle, DispatchPhase, DragMoveEvent,
    Entity, ExternalDragPayload, ExternalPaths, FileDragPaths, FocusHandle, Focusable, Hsla,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point, ScrollHandle,
    ScrollWheelEvent, ShapedLine, SharedString, TextRun, Transformation, UnderlineStyle, Window,
    canvas, div, fill, font, outline, point, prelude::*, px, radians, rgb, size, svg,
};
use parking_lot::Mutex;

use crate::assets;
use crate::collect::{self, CollectSource, COLLECT_JOB_TIMEOUT_SECS, COLLECT_MAX_BYTES, COLLECT_STEP_TIMEOUT_SECS};
use crate::history::HistoryStore;
use crate::log_highlight::LogHighlighter;
use crate::profiles::{AuthMethod, Profile, ProfileStore};
use crate::recents::{RecentEntry, RecentStore, now_unix, relative_time};
use crate::session::{
    Command as SessionCommand, Event as SessionEvent, FileEntry, SessionHandle, TempDownloadCache,
};
use crate::terminal_model::TerminalModel;
use crate::text_field::{Backtab, Tab, TextField};
use crate::theme;
use crate::api::{self, ApiRequest, Responder as ApiResponder};

const TERMINAL_FONT_SIZE: f32 = 13.0;
/// Line height tracks the (zoomable) font size with this ratio.
const TERMINAL_LINE_HEIGHT_RATIO: f32 = 1.35;
const HEADER_HEIGHT: f32 = 38.0;
const STATUSBAR_HEIGHT: f32 = 24.0;

/// Connection lifecycle shown in header/status bar.
#[derive(Clone, PartialEq)]
enum ConnState {
    Disconnected,
    Connecting,
    Connected,
}

/// One node of the lazily-loaded remote file tree.
#[derive(Clone)]
struct TreeNode {
    entry: FileEntry,
    expanded: bool,
    loading: bool,
    /// `None` for directories whose children have not been fetched yet.
    children: Option<Vec<TreeNode>>,
}

impl TreeNode {
    fn from_entry(entry: FileEntry) -> Self {
        Self {
            entry,
            expanded: false,
            loading: false,
            children: None,
        }
    }
}

/// Find a directory node by path (depth-first). Implemented in two phases —
/// locate the index path on an immutable walk, then descend mutably — to
/// stay within what the borrow checker accepts for tree walks.
fn find_node<'a>(nodes: &'a mut [TreeNode], path: &std::path::Path) -> Option<&'a mut TreeNode> {
    fn index_path(nodes: &[TreeNode], path: &std::path::Path) -> Option<Vec<usize>> {
        for (ix, node) in nodes.iter().enumerate() {
            if node.entry.path.as_path() == path {
                return Some(vec![ix]);
            }
            if let Some(children) = node.children.as_deref() {
                if let Some(mut sub) = index_path(children, path) {
                    let mut result = vec![ix];
                    result.append(&mut sub);
                    return Some(result);
                }
            }
        }
        None
    }

    fn at_mut<'a>(nodes: &'a mut [TreeNode], indices: &[usize]) -> Option<&'a mut TreeNode> {
        let (first, rest) = indices.split_first()?;
        let node = nodes.get_mut(*first)?;
        if rest.is_empty() {
            Some(node)
        } else {
            at_mut(node.children.as_deref_mut()?, rest)
        }
    }

    let indices = index_path(nodes, path)?;
    at_mut(nodes, &indices)
}

/// Re-list a directory in a tab's tree, showing the loading state.
fn reload_dir(tab: &mut SessionTab, path: &std::path::Path) {
    if let Some(node) = find_node(&mut tab.tree, path) {
        node.loading = true;
    }
    if let Some(session) = tab.session.as_ref() {
        session.list_dir(tab.session_id, path.to_path_buf());
    }
}

/// Payload of a drag started on a file-tree row.
#[derive(Clone)]
struct DraggedEntry {
    path: PathBuf,
    is_dir: bool,
    name: SharedString,
}

/// What is currently under the cursor during an internal tree drag, mirroring
/// Zed's project-panel `DragTarget`.
#[derive(Clone)]
enum TreeDragTarget {
    /// Hovering a row; drops land in the row's directory (or the row's
    /// parent directory when the row is a file).
    Row { path: PathBuf, is_dir: bool },
    /// Hovering the tree background; drops land in the tree root.
    Background,
}

/// Which view the sidebar shows.
#[derive(Clone, Copy, PartialEq)]
enum SidebarTab {
    Sessions,
    Files,
    Logs,
}

/// Severity of a tool-log entry.
#[derive(Clone, Copy, PartialEq)]
enum LogLevel {
    Info,
    Warn,
    Error,
}

/// One line of the tool log shown in the Logs sidebar tab.
struct LogEntry {
    /// Local time of the entry, `HH:MM:SS`.
    time: String,
    level: LogLevel,
    message: String,
}

/// Maximum number of log entries kept in memory.
const MAX_LOG_ENTRIES: usize = 1000;

/// Floating drag image shown next to the cursor while dragging a tree entry
/// (Zed's `DraggedProjectEntryView`).
struct DraggedEntryView {
    name: SharedString,
    is_dir: bool,
    click_offset: Point<Pixels>,
}

/// Payload for dragging the sidebar/terminal splitter; the drag image is
/// empty because the splitter itself follows the cursor.
struct SplitDrag;

struct SplitDragView;

impl Render for SplitDragView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl Render for DraggedEntryView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .pl(self.click_offset.x + px(12.))
            .pt(self.click_offset.y + px(12.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .items_center()
                    .py_1()
                    .px_2()
                    .rounded_lg()
                    .bg(theme::bg())
                    .border_1()
                    .border_color(theme::border())
                    .shadow_md()
                    .child(
                        svg()
                            .path(if self.is_dir {
                                assets::ICON_FOLDER
                            } else {
                                assets::ICON_FILE
                            })
                            .w(px(14.))
                            .h(px(14.))
                            .text_color(theme::text_dim()),
                    )
                    .child(div().text_sm().child(self.name.clone())),
            )
    }
}

/// Whether `profile` is missing credentials needed to connect: an empty
/// username, or (for password auth) an empty password. Key-file and agent
/// auth don't need a stored password, so they're never flagged here.
fn needs_login_prompt(profile: &Profile) -> bool {
    if profile.username.trim().is_empty() {
        return true;
    }
    matches!(&profile.auth, AuthMethod::Password { password } if password.is_empty())
}

/// Geometry shared between the terminal canvas (which knows its bounds and
/// cell metrics) and the poll task (which applies resizes).
#[derive(Clone, Copy)]
struct TermGeometry {
    bounds: Bounds<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
}

/// What a tab shows: an interactive shell session or a read-only `tail -f`
/// log follower owned by a shell tab.
enum TabKind {
    Shell,
    Log {
        /// The shell tab whose connection runs the tail.
        parent_id: u64,
        remote_path: PathBuf,
        /// Set when the tail channel ended (tab closed, file gone, or the
        /// session disconnected); the buffered content stays viewable.
        ended: bool,
    },
}

/// Per-tab state: one SSH session (shell tabs), one terminal grid, one file
/// tree. Cheap to create per tab; the heavy parts (`SessionHandle`'s backend
/// thread, `TerminalModel`'s grid) are owned here.
struct SessionTab {
    id: u64,
    kind: TabKind,
    /// The profile this shell tab is connecting/connected with.
    profile: Option<Profile>,
    state: ConnState,
    status: String,
    /// The backend connection; `None` for log tabs, which ride on their
    /// parent's connection.
    session: Option<SessionHandle>,
    terminal: TerminalModel,
    focus_handle: FocusHandle,
    tree: Vec<TreeNode>,
    root_path: Option<PathBuf>,
    /// Parent directory a `..` navigation is waiting a listing for.
    pending_tree_root: Option<PathBuf>,
    /// Incremented on each connect; stale dir listings from previous
    /// sessions are dropped when their tag mismatches.
    session_id: u64,
    /// Set when a session connects; applied in the next render (the event
    /// poll runs without a `Window`, so focusing is deferred).
    terminal_focus_pending: bool,
    geometry: Arc<Mutex<Option<TermGeometry>>>,
    /// Profile a connect is in flight for; recorded as a recent session once
    /// the `Connected` event arrives.
    connecting_profile: Option<Profile>,
    /// Currently selected tree path; the Download action acts on it.
    tree_selection: Option<PathBuf>,
    /// Focus handle for the file tree. Rows focus it on click, so tree
    /// shortcuts (Delete/Backspace) dispatch to the tree only while it has
    /// focus; the terminal keeps receiving keys otherwise.
    tree_focus_handle: FocusHandle,
    /// Active file transfer: (label, done bytes, total bytes, bytes/sec, eta seconds).
    transfer: Option<(String, u64, u64, f64, u64)>,
    /// Set to abort the active transfer; shared with the backend task.
    transfer_cancel: Option<Arc<AtomicBool>>,
    /// Remote directories that pending uploads write into; refreshed every
    /// time a transfer completes.
    pending_upload_dirs: Vec<PathBuf>,
    /// Log highlighting for `tail -f` tabs; `None` for shell tabs.
    highlighter: Option<Arc<LogHighlighter>>,
    /// Cross-session history recall position; `None` means the live line.
    history_pos: Option<usize>,
    /// SnakeTail-style search/filter/follow/bookmarks; `Some` only for log tabs.
    log_view: Option<LogView>,
}

impl SessionTab {
    fn is_log(&self) -> bool {
        matches!(self.kind, TabKind::Log { .. })
    }

    /// Effective lifecycle state for display: a finished log tab shows as
    /// disconnected even though its grid is still viewable.
    fn display_state(&self) -> ConnState {
        match &self.kind {
            TabKind::Log { ended: true, .. } => ConnState::Disconnected,
            _ => self.state.clone(),
        }
    }
}

/// SnakeTail-style view state for log-follow tabs: search, filter,
/// follow/pause, and bookmarks.
///
/// Rows are anchored by `line_index + lines_fed_at_capture` (see
/// `TerminalModel::lines_fed`): every newline shifts all existing rows'
/// alacritty line indexes by exactly one, so that sum is a stable identity
/// while the row stays in the grid. `line0_now = anchor - lines_fed_now`.
struct LogView {
    /// Pinned to the live edge; wheel-up (or the pause button) unpins so the
    /// stream keeps buffering while the user reads (SnakeTail's pause).
    follow: bool,
    search_field: Option<Entity<TextField>>,
    filter_field: Option<Entity<TextField>>,
    /// Text the caches below were computed for.
    search_text: String,
    filter_text: String,
    search_re: Option<regex::Regex>,
    filter_re: Option<regex::Regex>,
    /// Matching rows, oldest first: (anchor, match char ranges per row).
    matches: Vec<(i64, Vec<(usize, usize)>)>,
    /// Index into `matches` of the current match.
    current_match: Option<usize>,
    /// Anchors of rows matching the filter (oldest first); empty = filter
    /// yields nothing (or is inactive — check `filter_re`).
    filtered: Vec<i64>,
    /// Rows scrolled up from the live edge while filtering.
    filter_offset: usize,
    bookmarks: std::collections::BTreeSet<i64>,
    /// Current line, for bookmark toggling and navigation reference.
    caret: Option<i64>,
    /// Terminal output arrived; recompute matches/filter on next poll.
    stale: bool,
}

impl LogView {
    fn new() -> Self {
        Self {
            follow: true,
            search_field: None,
            filter_field: None,
            search_text: String::new(),
            filter_text: String::new(),
            search_re: None,
            filter_re: None,
            matches: Vec::new(),
            current_match: None,
            filtered: Vec::new(),
            filter_offset: 0,
            bookmarks: std::collections::BTreeSet::new(),
            caret: None,
            stale: true,
        }
    }

    /// Create the search/filter text fields on first use (needs a `Context`).
    fn ensure_fields(&mut self, cx: &mut Context<RootView>) {
        if self.search_field.is_none() {
            self.search_field = Some(cx.new(|cx| TextField::new(cx, "search…")));
        }
        if self.filter_field.is_none() {
            self.filter_field = Some(cx.new(|cx| TextField::new(cx, "filter…")));
        }
    }

    fn set_search(&mut self, text: String) {
        self.search_text = text;
        self.search_re = if self.search_text.is_empty() {
            None
        } else {
            regex::Regex::new(&format!("(?i){}", regex::escape(&self.search_text))).ok()
        };
        self.current_match = None;
        self.stale = true;
    }

    fn set_filter(&mut self, text: String) {
        self.filter_text = text;
        self.filter_re = if self.filter_text.is_empty() {
            None
        } else {
            regex::Regex::new(&format!("(?i){}", regex::escape(&self.filter_text))).ok()
        };
        self.filter_offset = 0;
        self.stale = true;
    }

    /// Recompute search matches and the filter row set from the grid, and
    /// prune anchors of rows that have scrolled out of the scrollback.
    fn refresh(&mut self, terminal: &TerminalModel) {
        self.stale = false;
        let fed = terminal.lines_fed() as i64;
        let (min_line0, screen) = terminal.grid_bounds();
        // Row indexes outlive the grid eventually; drop their bookmarks and
        // any current-match pointer that referenced them.
        self.bookmarks
            .retain(|&anchor| anchor - fed >= min_line0 as i64 - 1);
        self.matches.clear();
        self.filtered.clear();
        let search = self.search_re.clone();
        let filter = self.filter_re.clone();
        for line0 in min_line0..screen {
            let Some((text, cols)) = terminal.row_text(line0) else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            let anchor = line0 as i64 + fed;
            if let Some(re) = filter.as_ref() {
                if re.find(&text).is_some() {
                    self.filtered.push(anchor);
                }
            }
            if let Some(re) = search.as_ref() {
                let mut ranges = Vec::new();
                for found in re.find_iter(&text) {
                    let start_char = text[..found.start()].chars().count();
                    let end_char = text[..found.end()].chars().count();
                    if start_char >= end_char || end_char > cols.len() {
                        continue;
                    }
                    ranges.push((start_char, end_char));
                    if ranges.len() >= 64 {
                        break;
                    }
                }
                if !ranges.is_empty() {
                    self.matches.push((anchor, ranges));
                }
            }
        }
        if let Some(current) = self.current_match {
            if current >= self.matches.len() {
                self.current_match = None;
            }
        }
        // Keep the caret alive only while its row is in the grid.
        if let Some(caret) = self.caret {
            if caret - fed < min_line0 as i64 || caret - fed >= screen as i64 {
                self.caret = None;
            }
        }
    }

    /// `true` when only filter-matching rows should render.
    fn filtering(&self) -> bool {
        self.filter_re.is_some()
    }

    /// Current line for bookmark toggling: explicit caret, else the current
    /// match, else the newest grid row.
    fn caret_anchor(&self, terminal: &TerminalModel) -> i64 {
        let fed = terminal.lines_fed() as i64;
        if let Some(caret) = self.caret {
            return caret;
        }
        if let Some(current) = self.current_match {
            return self.matches[current].0;
        }
        // Newest content row: the topmost line index minus one (the top row
        // is usually the empty cursor line).
        let (_min, screen) = terminal.grid_bounds();
        fed + screen as i64 - 2
    }
}

/// Per-frame painting data for a log tab's overlays, built on the UI thread
/// and passed into the canvas prepaint.
struct LogOverlay {    /// Search-match char ranges per alacritty line index.
    matches: std::collections::HashMap<i32, Vec<(usize, usize)>>,
    /// The active match gets a stronger background.
    current: Option<(i32, (usize, usize))>,
    /// Bookmarked line indexes.
    bookmarks: Vec<i32>,
    /// Current-line marker.
    caret: Option<i32>,
}

impl LogOverlay {
    fn build(view: &LogView, fed: i64) -> Self {
        let mut matches = std::collections::HashMap::new();
        let mut current = None;
        for (ix, (anchor, ranges)) in view.matches.iter().enumerate() {
            let line0 = (*anchor - fed) as i32;
            if Some(ix) == view.current_match {
                current = Some((line0, ranges[0]));
            }
            matches.insert(line0, ranges.clone());
        }
        Self {
            matches,
            current,
            bookmarks: view
                .bookmarks
                .iter()
                .map(|anchor| (*anchor - fed) as i32)
                .collect(),
            caret: view.caret.map(|anchor| (anchor - fed) as i32),
        }
    }
}

/// Which auth method the profile form is editing.
#[derive(Clone, Copy, PartialEq)]
enum AuthKind {
    Password,
    KeyFile,
    Agent,
}

/// A REST API call awaiting its backend reply, keyed by request id.
enum ApiWait {
    Exec {
        reply: ApiResponder,
        deadline: std::time::Instant,
    },
    List {
        reply: ApiResponder,
        deadline: std::time::Instant,
    },
    Upload {
        reply: ApiResponder,
        deadline: std::time::Instant,
    },
    Download {
        reply: ApiResponder,
        deadline: std::time::Instant,
    },
    /// One step of a `/logs/collect` job; the job itself holds the reply.
    CollectStep {
        deadline: std::time::Instant,
    },
}

/// A `POST /logs/collect` job in progress: sources run one at a time over
/// the target's session (each step reuses the ApiExec machinery, so only
/// one extra exec channel is open at a time), and the per-source outcomes
/// accumulate into the final reply.
struct CollectJob {
    target: String,
    pending: std::collections::VecDeque<CollectSource>,
    current: Option<CollectSource>,
    outcomes: Vec<serde_json::Value>,
    reply: ApiResponder,
    deadline: std::time::Instant,
}

impl ApiWait {
    fn deadline(&self) -> std::time::Instant {
        match self {
            ApiWait::Exec { deadline, .. }
            | ApiWait::List { deadline, .. }
            | ApiWait::Upload { deadline, .. }
            | ApiWait::Download { deadline, .. }
            | ApiWait::CollectStep { deadline } => *deadline,
        }
    }

    fn fail(self, message: &str) {
        let reply = match self {
            ApiWait::Exec { reply, .. }
            | ApiWait::List { reply, .. }
            | ApiWait::Upload { reply, .. }
            | ApiWait::Download { reply, .. } => Some(reply),
            ApiWait::CollectStep { .. } => None,
        };
        if let Some(reply) = reply {
            let _ = reply.send(serde_json::json!({"ok": false, "error": message}));
        }
    }
}

/// A `/logs` request whose target session is still connecting.
struct PendingApiLog {
    target: String,
    path: PathBuf,
    reply: ApiResponder,
    deadline: std::time::Instant,
}

/// Which local editor to open a staged file with.
#[derive(Clone, Copy, PartialEq)]
enum EditorChoice {
    /// The OS default editor (`open`, `xdg-open`, Explorer) or
    /// `$AETHERIUM_EDITOR` when set.
    Default,
    /// VS Code explicitly.
    VsCode,
}

/// An editor launch waiting for its staging download to finish.
struct PendingEditorOpen {
    session_id: u64,
    remote: PathBuf,
    editor: EditorChoice,
}

/// A remote file open in a local editor (MobaXterm-style remote editing):
/// the temp copy is watched, and every save asks whether to sync back.
struct RemoteEdit {
    session_id: u64,
    remote: PathBuf,
    local: PathBuf,
    /// Profile summary, for the sync dialog text.
    target: String,
    mtime: std::time::SystemTime,
    size: u64,
}

/// The pending "upload back to the device?" question for a modified temp
/// copy.
#[derive(Clone)]
struct EditSyncAsk {
    session_id: u64,
    remote: PathBuf,
    local: PathBuf,
    target: String,
}

/// State of the inline profile add/edit form.
struct ProfileForm {
    editing: Option<usize>,
    auth_kind: AuthKind,
    name: Entity<TextField>,
    host: Entity<TextField>,
    port: Entity<TextField>,
    username: Entity<TextField>,
    password: Entity<TextField>,
    key_path: Entity<TextField>,
    passphrase: Entity<TextField>,
}

/// Inline text editor in the file tree: renames an entry (`target = Some`)
/// or creates a new file/dir inside `parent` (`target = None`). Enter
/// submits, Escape cancels; both are raw keys handled by the row container.
struct TreeEditor {
    target: Option<PathBuf>,
    parent: PathBuf,
    is_dir: bool,
    field: Entity<TextField>,
}

pub struct RootView {
    store: ProfileStore,
    selected: Option<usize>,
    form: Option<ProfileForm>,
    /// Recently connected sessions (persisted).
    recents: RecentStore,
    /// Global status for UI-level messages (profile saved, …); each tab has
    /// its own session status.
    status: String,
    tabs: Vec<SessionTab>,
    active: usize,
    next_tab_id: u64,
    /// Right-click menu in the file tree: click position + file path.
    /// Right-click context menu: position, path, and whether the path is a
    /// directory (directory menus offer the ZIP download).
    context_menu: Option<(Point<Pixels>, PathBuf, bool)>,
    /// Delete confirmation dialog: the path awaiting a final "Delete" click.
    /// Deleting is permanent over SFTP (no trash), so both the Delete key
    /// and the context menu ask first.
    confirm_delete: Option<PathBuf>,
    /// The theme switcher dropdown is open (anchored under the header).
    theme_menu: bool,
    /// Inline rename/create editor in the file tree, if one is open.
    tree_editor: Option<TreeEditor>,
    /// Entry under the cursor during an internal file-tree drag.
    tree_drag_target: Option<TreeDragTarget>,
    /// Entry being dragged in the file tree (set when a drag starts moving).
    tree_dragging: Option<DraggedEntry>,
    /// Windows-only: our own OLE drag owns the pointer, so gpui's internal
    /// file-tree drop/hover handling must stand down (set when the OLE drag
    /// starts, cleared on the next real mouse-down).
    ole_drag_active: bool,
    /// Incoming REST API requests (drained by the event loop).
    api_rx: std::sync::mpsc::Receiver<ApiRequest>,
    /// REST API calls awaiting their backend replies.
    api_pending: std::collections::HashMap<u64, ApiWait>,
    /// A `/logs/collect` job in progress, if any (one at a time).
    api_collect: Option<CollectJob>,
    next_api_req: u64,
    /// A `/logs` request waiting for its session to connect.
    api_pending_log: Option<PendingApiLog>,
    /// Remote files open in a local editor, watched for writes.
    remote_edits: Vec<RemoteEdit>,
    /// Editor opens waiting for their staging download.
    pending_editor_open: Vec<PendingEditorOpen>,
    /// The "sync back?" dialog state, one question at a time.
    edit_sync_ask: Option<EditSyncAsk>,
    last_edit_scan: std::time::Instant,
    /// Which sidebar view is shown.
    sidebar_tab: SidebarTab,
    /// Current width of the sidebar, dragged via the splitter.
    sidebar_width: Pixels,
    /// Whether file rows show a details column (size).
    show_file_details: bool,
    /// The tool's own log (connection issues, transfer errors, …).
    logs: Vec<LogEntry>,
    log_scroll: ScrollHandle,
    /// Set when new log entries arrived (or the tab was opened); the log
    /// list scrolls to the bottom on the next render.
    logs_scroll_pending: bool,
    /// Wake channel shared by all tab terminals: when any terminal becomes
    /// dirty, the SSH reader thread sends on this to wake the repaint loop
    /// immediately instead of waiting for a fixed poll tick.
    terminal_wake_tx: tokio::sync::mpsc::UnboundedSender<()>,
    /// Cache of (session, remote path) → local temp path for drag-out.
    /// Populated by the staging download directly on the backend thread (see
    /// [`TempDownloadCache`]) so the `external_drag_payload` resolver can
    /// block on it while the drag is leaving the window; the
    /// `TempDownloadReady` event additionally drives status text and the
    /// "Open in VS Code" flow. Entries are deleted (and their files removed)
    /// when the drag ends inside the app, when the session disconnects or its
    /// tab closes, and every staged file is purged on the next app launch.
    temp_download_cache: TempDownloadCache,
    /// Instant local echo of printable keystrokes (the server's identical
    /// echo is deduplicated on arrival) — the fix for typing lag on
    /// high-latency links. Toggleable from the header.
    local_echo: bool,
    /// Zoomable terminal font size (cmd/ctrl +/-, cmd/ctrl+wheel); the grid
    /// re-measures and the PTY resizes automatically. Persisted across
    /// launches in `ui.toml`.
    terminal_font_size: f32,
    /// Heuristic syntax coloring for shell output the remote program left
    /// uncolored; loaded once at startup (rules from `shell_highlight.toml`).
    shell_highlighter: Arc<LogHighlighter>,
    /// Whether shell syntax coloring is applied; header toggle, persisted.
    shell_coloring: bool,
    /// The shortcuts & features overlay (header help button / Escape).
    help_open: bool,
    /// Commands submitted in any session, persisted across launches and
    /// recallable with Shift+↑ / Shift+↓ (roadmap 3.4).
    history: HistoryStore,
    focus_handle: FocusHandle,
}

/// Persisted UI knobs: the terminal font zoom and the shell coloring
/// toggle. Hand-parsed `key = value` lines, same style as `theme.toml`.
struct UiSettings {
    terminal_font_size: f32,
    shell_coloring: bool,
}

impl UiSettings {
    fn defaults() -> Self {
        Self {
            terminal_font_size: TERMINAL_FONT_SIZE,
            shell_coloring: true,
        }
    }

    fn load() -> Self {
        let mut settings = Self::defaults();
        let Ok(text) = std::fs::read_to_string(crate::crypto::config_dir().join("ui.toml"))
        else {
            return settings;
        };
        for (key, value) in text.lines().filter_map(|line| line.split_once('=')) {
            match key.trim() {
                "terminal_font_size" => {
                    if let Ok(size) = value.trim().parse::<f32>() {
                        settings.terminal_font_size = size.clamp(8., 32.);
                    }
                }
                "shell_coloring" => settings.shell_coloring = value.trim() != "false",
                _ => {}
            }
        }
        settings
    }

    fn save(&self) {
        if let Err(err) = std::fs::write(
            crate::crypto::config_dir().join("ui.toml"),
            format!(
                "terminal_font_size = {}\nshell_coloring = {}\n",
                self.terminal_font_size, self.shell_coloring
            ),
        ) {
            eprintln!("aetherium: saving ui settings: {err:#}");
        }
    }
}

impl RootView {
    pub fn new(
        cx: &mut Context<Self>,
        api_rx: std::sync::mpsc::Receiver<ApiRequest>,
        api_info: Option<api::ApiInfo>,
    ) -> Self {
        let (terminal_wake_tx, terminal_wake_rx) =
            tokio::sync::mpsc::unbounded_channel::<()>();
        let store = ProfileStore::load();
        let ui_settings = UiSettings::load();
        // Pre-select the first profile so Connect works with one click.
        let selected = (!store.profiles.is_empty()).then_some(0);
        let view = Self {
            store,
            selected,
            form: None,
            recents: RecentStore::load(),
            status: match api_info {
                Some(ref info) => format!("REST API: {}", api::url(info)),
                None => "not connected".to_string(),
            },
            tabs: Vec::new(),
            active: 0,
            next_tab_id: 0,
            context_menu: None,
            confirm_delete: None,
            theme_menu: false,
            tree_editor: None,
            tree_drag_target: None,
            tree_dragging: None,
            ole_drag_active: false,
            api_rx,
            api_pending: std::collections::HashMap::new(),
            api_collect: None,
            next_api_req: 1,
            api_pending_log: None,
            remote_edits: Vec::new(),
            pending_editor_open: Vec::new(),
            edit_sync_ask: None,
            last_edit_scan: std::time::Instant::now(),
            sidebar_tab: SidebarTab::Sessions,
            sidebar_width: px(260.),
            show_file_details: false,
            logs: Vec::new(),
            log_scroll: ScrollHandle::default(),
            logs_scroll_pending: false,
            terminal_wake_tx,
            temp_download_cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
            local_echo: true,
            terminal_font_size: ui_settings.terminal_font_size,
            shell_highlighter: Arc::new(LogHighlighter::load_shell()),
            shell_coloring: ui_settings.shell_coloring,
            help_open: false,
            history: HistoryStore::load(),
            focus_handle: cx.focus_handle(),
        };
        // Purge staging leftovers from previous runs (drag-out cancels,
        // interrupted transfers). The directory is private to this app.
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("aetherium"));
        view.spawn_repaint_loop(cx, terminal_wake_rx);
        view.spawn_event_loop(cx);
        view
    }

    fn alloc_tab_id(&mut self) -> u64 {
        self.next_tab_id += 1;
        self.next_tab_id
    }

    fn active_tab(&self) -> Option<&SessionTab> {
        self.tabs.get(self.active)
    }

    fn active_tab_mut(&mut self) -> Option<&mut SessionTab> {
        let active = self.active;
        self.tabs.get_mut(active)
    }

    fn tab_index_by_id(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.id == id)
    }

    /// Repaint as soon as any terminal wakes us, with a 16ms timer as a
    /// fallback for periodic bookkeeping (geometry/resize sync) and to catch
    /// any wake we might have missed. Terminal output wakes instantly, so
    /// typing echoes with no polling delay.
    fn spawn_repaint_loop(
        &self,
        cx: &mut Context<Self>,
        mut wake_rx: tokio::sync::mpsc::UnboundedReceiver<()>,
    ) {
        cx.spawn(async move |this, cx| {
            loop {
                // Wait for a terminal wake or the 16ms fallback tick, whichever
                // comes first.
                let tick = cx.background_executor().timer(Duration::from_millis(16));
                tokio::select! {
                    _ = tick => {}
                    _ = wake_rx.recv() => {}
                }
                // Drain any pending wake signals so a burst of output doesn't
                // queue up extra repaints.
                while wake_rx.try_recv().is_ok() {}
                let result = this.update(cx, |this, cx| {
                    // Apply terminal resizes based on the canvas geometry of
                    // the active tab; inactive grids keep their size and are
                    // re-measured by their prepaint once activated.
                    let active = this.active;
                    if let Some(tab) = this.tabs.get_mut(active) {
                        if let Some(geometry) = *tab.geometry.lock() {
                            let cols =
                                (f32::from(geometry.bounds.size.width) / f32::from(geometry.cell_width))
                                    .floor() as usize;
                            let rows = (f32::from(geometry.bounds.size.height)
                                / f32::from(geometry.line_height))
                                .floor() as usize;
                            if tab.terminal.resize(cols, rows) {
                                if let Some(session) = tab.session.as_ref() {
                                    session.resize_pty(cols as u32, rows as u32);
                                }
                            }
                        }
                    }
                    if this
                        .tabs
                        .iter()
                        .any(|tab| tab.terminal.take_dirty())
                    {
                        // New output invalidates log search/filter caches.
                        for tab in &mut this.tabs {
                            if let Some(view) = tab.log_view.as_mut() {
                                view.stale = true;
                            }
                        }
                        cx.notify();
                    }
                });
                if result.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// 50ms poll of session events (dir listings, connect/disconnect, tails).
    /// Every tab drains its own backend handle, so plain events are already
    /// routed to the right tab; tail events carry the target tab id.
    fn spawn_event_loop(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                let result = this.update(cx, |this, cx| {
                    let mut handled = false;
                    for index in 0..this.tabs.len() {
                        let Some(session) = this.tabs[index].session.clone() else {
                            continue;
                        };
                        while let Some(event) = session.try_recv_event() {
                            this.handle_tab_event(index, event, cx);
                            handled = true;
                        }
                    }
                    // REST API requests from the local HTTP layer.
                    while let Ok(request) = this.api_rx.try_recv() {
                        this.handle_api_request(request, cx);
                        handled = true;
                    }
                    this.api_tick(cx);
                    this.log_view_upkeep(cx);
                    this.edit_scan(cx);
                    if handled {
                        cx.notify();
                    }
                });
                if result.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Per-tick upkeep of the active log tab: follow pinning, applying
    /// search/filter field text, and refreshing stale result caches.
    fn log_view_upkeep(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let Some(view) = tab.log_view.as_mut() else {
            return;
        };
        // Pin to the live edge while following.
        if view.follow {
            if view.filtering() {
                view.filter_offset = 0;
            } else {
                tab.terminal.term.lock().scroll_display(Scroll::Bottom);
            }
        }
        // The toolbar fields edit themselves; pick up their text here so the
        // whole view updates even though the fields re-render independently.
        let search_now = view
            .search_field
            .as_ref()
            .map(|field| field.read(cx).text().to_string())
            .unwrap_or_default();
        if search_now != view.search_text {
            view.set_search(search_now);
        }
        let filter_now = view
            .filter_field
            .as_ref()
            .map(|field| field.read(cx).text().to_string())
            .unwrap_or_default();
        if filter_now != view.filter_text {
            view.set_filter(filter_now);
        }
        if view.stale {
            view.refresh(&tab.terminal);
            tab.terminal.mark_dirty();
            cx.notify();
        }
    }

    fn handle_tab_event(&mut self, index: usize, event: SessionEvent, cx: &mut Context<Self>) {
        match event {
            SessionEvent::Connected { home_dir } => {
                let tab = &mut self.tabs[index];
                tab.state = ConnState::Connected;
                tab.status = format!("connected, home: {}", home_dir.display());
                tab.terminal_focus_pending = true;
                tab.root_path = Some(home_dir.clone());
                tab.tree = vec![TreeNode {
                    entry: FileEntry {
                        name: home_dir.display().to_string(),
                        path: home_dir.clone(),
                        is_dir: true,
                        size: 0,
                        modified: None,
                    },
                    expanded: true,
                    loading: true,
                    children: None,
                }];
                let session_id = tab.session_id;
                tab.session
                    .as_ref()
                    .expect("shell tab has a session")
                    .list_dir(session_id, home_dir.clone());
                // Remember the session in the recent-sessions list.
                let profile = tab.connecting_profile.take();
                if let Some(profile) = profile {
                    self.recents.record(RecentEntry {
                        profile_name: profile.name.clone(),
                        host: profile.host.clone(),
                        username: profile.username.clone(),
                        port: profile.port,
                        connected_at_unix: now_unix(),
                    });
                    if let Err(err) = self.recents.save() {
                        eprintln!("zedterm: failed to save recents: {err:#}");
                        self.log(LogLevel::Warn, format!("failed to save recents: {err:#}"));
                    }
                }
                let label = Self::tab_log_label(&self.tabs[index]);
                self.log(
                    LogLevel::Info,
                    format!("{label}: connected (home: {})", home_dir.display()),
                );
                log::info!("connect: {label} connected (home: {})", home_dir.display());
            }
            SessionEvent::DirListing { session_id, path, entries } => {
                let tab = &mut self.tabs[index];
                // A listing from a previous session must not land in the
                // freshly connected tree.
                if session_id != tab.session_id {
                    return;
                }
                let mut entries: Vec<TreeNode> = entries.into_iter().map(TreeNode::from_entry).collect();
                // Directories first, then alphabetical.
                entries.sort_by(|a, b| {
                    b.entry
                        .is_dir
                        .cmp(&a.entry.is_dir)
                        .then_with(|| a.entry.name.to_lowercase().cmp(&b.entry.name.to_lowercase()))
                });
                // A `..` navigation: the listed directory becomes the new
                // tree root, keeping the previous root visible (expanded)
                // inside it.
                if tab.pending_tree_root.take().as_deref() == Some(path.as_path()) {
                    let parent_entry = FileEntry {
                        name: path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.display().to_string()),
                        path: path.clone(),
                        is_dir: true,
                        size: 0,
                        modified: None,
                    };
                    if let Some(old_root) = tab.tree.first().cloned() {
                        if let Some(slot) = entries
                            .iter_mut()
                            .find(|node| node.entry.path == old_root.entry.path)
                        {
                            *slot = old_root;
                        }
                    }
                    tab.tree = vec![TreeNode {
                        entry: parent_entry,
                        expanded: true,
                        loading: false,
                        children: Some(entries),
                    }];
                    tab.root_path = Some(path.clone());
                    return;
                }
                if let Some(node) = find_node(&mut tab.tree, &path) {
                    node.children = Some(entries);
                    node.loading = false;
                    node.expanded = true;
                }
            }
            SessionEvent::TransferStarted { label } => {
                self.tabs[index].transfer = Some((label.clone(), 0, 0, 0.0, 0));
                let tab_label = Self::tab_log_label(&self.tabs[index]);
                self.log(LogLevel::Info, format!("{tab_label}: {label}"));
            }
            SessionEvent::TransferProgress { label, done_bytes, total_bytes, bytes_per_second, eta_seconds } => {
                self.tabs[index].transfer = Some((label, done_bytes, total_bytes, bytes_per_second, eta_seconds));
            }
            SessionEvent::TransferDone { label } => {
                let tab = &mut self.tabs[index];
                tab.transfer = None;
                tab.transfer_cancel = None;
                tab.status = label.clone();
                // Refresh every directory that pending uploads write into.
                let dirs = std::mem::take(&mut tab.pending_upload_dirs);
                let session_id = tab.session_id;
                if let Some(session) = tab.session.as_ref() {
                    for dir in &dirs {
                        if let Some(node) = find_node(&mut tab.tree, dir) {
                            node.loading = true;
                        }
                        session.list_dir(session_id, dir.clone());
                    }
                }
                let tab_label = Self::tab_log_label(&self.tabs[index]);
                self.log(LogLevel::Info, format!("{tab_label}: {label}"));
            }
            SessionEvent::TransferCancelled { label } => {
                let tab = &mut self.tabs[index];
                tab.transfer = None;
                tab.transfer_cancel = None;
                tab.status = format!("cancelled {label}");
                // Refresh every directory pending uploads wrote into, like a
                // finished transfer (completed files stay, partials are gone).
                let dirs = std::mem::take(&mut tab.pending_upload_dirs);
                let session_id = tab.session_id;
                if let Some(session) = tab.session.as_ref() {
                    for dir in &dirs {
                        if let Some(node) = find_node(&mut tab.tree, dir) {
                            node.loading = true;
                        }
                        session.list_dir(session_id, dir.clone());
                    }
                }
                let tab_label = Self::tab_log_label(&self.tabs[index]);
                self.log(LogLevel::Warn, format!("{tab_label}: cancelled {label}"));
            }
            SessionEvent::EntryMoved { from, to } => {
                let tab = &mut self.tabs[index];
                tab.status = format!("moved {} → {}", from.display(), to.display());
                if tab.tree_selection.as_ref() == Some(&from) {
                    tab.tree_selection = Some(to.clone());
                }
                self.tree_drag_target = None;
                self.tree_dragging = None;
                // Refresh both the directory that lost the entry and the one
                // that gained it (the moved subtree reloads collapsed).
                let old_parent = from.parent().map(PathBuf::from);
                let new_parent = to.parent().map(PathBuf::from);
                if let Some(parent) = old_parent.as_ref() {
                    reload_dir(tab, parent);
                }
                if let Some(parent) = new_parent.as_ref() {
                    if old_parent.as_ref() != Some(parent) {
                        reload_dir(tab, parent);
                    }
                }
                let tab_label = Self::tab_log_label(&self.tabs[index]);
                self.log(
                    LogLevel::Info,
                    format!("{tab_label}: moved {} → {}", from.display(), to.display()),
                );
            }
            SessionEvent::EntryDeleted { path } => {
                // A staged temp copy of the deleted file is useless now;
                // drop it like a drag that ended inside the app.
                let session_id = self.tabs[index].session_id;
                if let Some(local) = self
                    .temp_download_cache
                    .lock()
                    .remove(&(session_id, path.clone()))
                {
                    if let Some(dir) = local.parent() {
                        let _ = std::fs::remove_dir_all(dir);
                    }
                }
                let tab = &mut self.tabs[index];
                tab.status = format!("deleted {}", path.display());
                if tab.tree_selection.as_ref() == Some(&path) {
                    tab.tree_selection = None;
                }
                // Refresh the directory that contained the entry.
                if let Some(parent) = path.parent().map(PathBuf::from) {
                    reload_dir(tab, &parent);
                }
                let tab_label = Self::tab_log_label(&self.tabs[index]);
                self.log(
                    LogLevel::Info,
                    format!("{tab_label}: deleted {}", path.display()),
                );
            }
            SessionEvent::EntryCreated { parent } => {
                let tab = &mut self.tabs[index];
                tab.status = format!("created {}", parent.display());
                reload_dir(tab, &parent);
                let tab_label = Self::tab_log_label(&self.tabs[index]);
                self.log(
                    LogLevel::Info,
                    format!("{tab_label}: created something in {}", parent.display()),
                );
            }
            SessionEvent::Error(message) => {
                let tab = &mut self.tabs[index];
                tab.status = message.clone();
                if tab.state == ConnState::Connecting {
                    tab.state = ConnState::Disconnected;
                }
                let tab_label = Self::tab_log_label(&self.tabs[index]);
                self.log(LogLevel::Error, format!("{tab_label}: {message}"));
                log::warn!("session: {tab_label}: {message}");
            }
            SessionEvent::Disconnected => {
                let tab_id = self.tabs[index].id;
                let session_id = self.tabs[index].session_id;
                let tab = &mut self.tabs[index];
                tab.state = ConnState::Disconnected;
                tab.status = "disconnected".to_string();
                tab.tree.clear();
                tab.root_path = None;
                tab.tree_selection = None;
                tab.transfer = None;
                tab.transfer_cancel = None;
                tab.pending_upload_dirs.clear();
                tab.connecting_profile = None;
                self.tree_drag_target = None;
                self.tree_dragging = None;
                // Drop the old session's output; focus stays on the sidebar.
                tab.terminal.reset();
                // Drag-out staging for this session is useless now; delete
                // the staged files (uuid dirs under the temp area).
                self.purge_staged_for(session_id);
                // The confirmation dialog (if up) outlived its session.
                self.confirm_delete = None;
                self.tree_editor = None;
                // The tail channels die with the connection; mark the log
                // tabs that rode on it.
                for child in &mut self.tabs {
                    if let TabKind::Log { parent_id, ended, .. } = &mut child.kind {
                        if *parent_id == tab_id && !*ended {
                            *ended = true;
                            child.status = "session closed".to_string();
                        }
                    }
                }
                let tab_label = Self::tab_log_label(&self.tabs[index]);
                self.log(LogLevel::Warn, format!("{tab_label}: disconnected"));
                log::info!("session: {tab_label} disconnected");
            }
            SessionEvent::TempDownloadReady {
                session_id,
                remote,
                local,
            } => {
                let mut cache = self.temp_download_cache.lock();
                if let Some(old) = cache.insert((session_id, remote.clone()), local.clone()) {
                    // Superseded staging of the same file: drop the old copy
                    // — but not when it's this same path; the backend now
                    // publishes into the cache before this event arrives, so
                    // `old == local` is the normal case, and deleting it
                    // would remove the live staged file.
                    if old != local {
                        if let Some(dir) = old.parent() {
                            let _ = std::fs::remove_dir_all(dir);
                        }
                    }
                }
                drop(cache);
                log::info!("drag-out: staged {} (session {session_id})", remote.display());
                self.tabs[index].status = format!("staged {} for drag-out", remote.display());
                // An editor request ("Edit locally" / "Open in VS Code") is
                // satisfied by the same staging download; the file is now
                // watched for writes.
                if let Some(position) = self
                    .pending_editor_open
                    .iter()
                    .position(|pending| pending.session_id == session_id && pending.remote == remote)
                {
                    let pending = self.pending_editor_open.remove(position);
                    let target = self.tabs[index]
                        .profile
                        .as_ref()
                        .map(|profile| profile.summary())
                        .unwrap_or_default();
                    self.begin_remote_edit(
                        session_id,
                        remote,
                        local,
                        target,
                        pending.editor,
                        cx,
                    );
                }
            }
            SessionEvent::TailEnded { tab_id } => {
                self.end_log_tab(tab_id, "tail ended");
                let label = Self::tab_log_label(&self.tabs[index]);
                self.log(LogLevel::Info, format!("{label}: tail ended"));
            }
            SessionEvent::TailError { tab_id, message } => {
                self.end_log_tab(tab_id, message.clone());
                let label = Self::tab_log_label(&self.tabs[index]);
                self.log(LogLevel::Error, format!("{label}: {message}"));
            }
            // REST API replies: route to the waiting HTTP request by id.
            SessionEvent::ApiExecDone { req_id, stdout, stderr, exit_status } => {
                match self.api_pending.remove(&req_id) {
                    Some(ApiWait::Exec { reply, .. }) => {
                        let mut value = serde_json::json!({
                            "ok": true,
                            "stdout": String::from_utf8_lossy(&stdout),
                            "stderr": String::from_utf8_lossy(&stderr),
                            "exit_status": exit_status,
                        });
                        if std::str::from_utf8(&stdout).is_err() {
                            use base64::Engine as _;
                            value["stdout_base64"] =
                                serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode(&stdout));
                        }
                        if std::str::from_utf8(&stderr).is_err() {
                            use base64::Engine as _;
                            value["stderr_base64"] =
                                serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode(&stderr));
                        }
                        let _ = reply.send(value);
                    }
                    Some(ApiWait::CollectStep { .. }) => {
                        self.collect_step_done(stdout, stderr, exit_status);
                    }
                    _ => {}
                }
            }
            SessionEvent::ApiListDone { req_id, result } => {
                if let Some(ApiWait::List { reply, .. }) = self.api_pending.remove(&req_id) {
                    let value = match result {
                        Ok(entries) => serde_json::json!({
                            "ok": true,
                            "entries": entries.iter().map(|entry| serde_json::json!({
                                "name": entry.name,
                                "path": entry.path.display().to_string(),
                                "is_dir": entry.is_dir,
                                "size": entry.size,
                                "modified": entry.modified,
                            })).collect::<Vec<_>>(),
                        }),
                        Err(message) => serde_json::json!({"ok": false, "error": message}),
                    };
                    let _ = reply.send(value);
                }
            }
            SessionEvent::ApiUploadDone { req_id, result } => {
                if let Some(ApiWait::Upload { reply, .. }) = self.api_pending.remove(&req_id) {
                    let value = match result {
                        Ok(bytes) => serde_json::json!({"ok": true, "bytes": bytes}),
                        Err(message) => serde_json::json!({"ok": false, "error": message}),
                    };
                    let _ = reply.send(value);
                }
            }
            SessionEvent::ApiDownloadDone { req_id, result } => {
                if let Some(ApiWait::Download { reply, .. }) = self.api_pending.remove(&req_id) {
                    let value = match result {
                        Ok((local, size)) => match std::fs::read(&local) {
                            Ok(bytes) => {
                                let _ = std::fs::remove_file(&local);
                                use base64::Engine as _;
                                serde_json::json!({
                                    "ok": true,
                                    "size": size,
                                    "content_base64": base64::engine::general_purpose::STANDARD.encode(&bytes),
                                })
                            }
                            Err(err) => serde_json::json!({"ok": false, "error": format!("reading {}: {err}", local.display())}),
                        },
                        Err(message) => serde_json::json!({"ok": false, "error": message}),
                    };
                    let _ = reply.send(value);
                }
            }
        }
    }

    /// Mark a log tab as finished and set its status message.
    fn end_log_tab(&mut self, tab_id: u64, message: impl Into<String>) {
        if let Some(index) = self.tab_index_by_id(tab_id) {
            if let TabKind::Log { ended, .. } = &mut self.tabs[index].kind {
                *ended = true;
            }
            self.tabs[index].status = message.into();
        }
    }

    // --- actions -----------------------------------------------------------

// [impl->feat~connection-profiles~1]
    fn connect_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.store.profiles.is_empty() {
            // Nothing to connect to yet — offer the profile form instead of
            // an easy-to-miss status message.
            self.open_new_profile_form(window, cx);
            return;
        }
        let Some(profile) = self.selected.and_then(|i| self.store.profiles.get(i)).cloned()
        else {
            self.status = "select a profile first".into();
            cx.notify();
            return;
        };
        log::info!("connect: connecting to {}", profile.summary());
        self.connect_profile(profile, window, cx);
    }

    /// Open a new shell tab and connect it to `profile`; if the username or
    /// (for password auth) the password is missing, open the profile form
    /// instead so the user can fill in credentials before connecting.
    fn connect_profile(&mut self, profile: Profile, window: &mut Window, cx: &mut Context<Self>) {
        if needs_login_prompt(&profile) {
            let editing = self.store.profiles.iter().position(|p| {
                p.host == profile.host && p.port == profile.port && p.username == profile.username
            });
            log::info!("connect: {} needs credentials, opening the form", profile.summary());
            self.status = format!("enter credentials for {}", profile.summary());
            self.form = Some(ProfileForm::from_profile(editing, &profile, cx));
            self.focus_first_form_field(window, cx);
            cx.notify();
            return;
        }
        self.spawn_shell_tab(profile, cx);
        self.context_menu = None;
    }

    /// Build, connect, and activate a visible shell tab; returns its index.
    /// Shared by the Connect button and the REST API's `/sessions`.
    fn spawn_shell_tab(&mut self, profile: Profile, cx: &mut Context<Self>) -> usize {
        let terminal = TerminalModel::new(80, 24);
        terminal.set_wake_channel(Some(self.terminal_wake_tx.clone()));
        let session = SessionHandle::spawn(terminal.clone());
        let id = self.alloc_tab_id();
        self.log(
            LogLevel::Info,
            format!("connecting to {}…", profile.summary()),
        );
        let tab = SessionTab {
            id,
            kind: TabKind::Shell,
            profile: Some(profile.clone()),
            state: ConnState::Connecting,
            status: format!("connecting to {}…", profile.summary()),
            session: Some(session.clone()),
            terminal,
            focus_handle: cx.focus_handle(),
            tree: Vec::new(),
            root_path: None,
            pending_tree_root: None,
            session_id: 1,
            terminal_focus_pending: false,
            geometry: Arc::new(Mutex::new(None)),
            connecting_profile: Some(profile.clone()),
            tree_selection: None,
            tree_focus_handle: cx.focus_handle(),
            transfer: None,
            transfer_cancel: None,
            pending_upload_dirs: Vec::new(),
            highlighter: None,
            history_pos: None,
            log_view: None,
        };
        session.connect(profile);
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        cx.notify();
        self.tabs.len() - 1
    }

    /// Open a new read-only tab that follows a remote file via `tail -f`,
    /// running on the active (parent) tab's connection.
// [impl->feat~log-follow-view~1]
    fn open_log_tab(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let connected = self
            .active_tab()
            .is_some_and(|tab| tab.state == ConnState::Connected && !tab.is_log());
        if !connected {
            self.status = "connect a shell tab before tailing a file".into();
            cx.notify();
            return;
        }
        let parent_index = self.active;
        let id = self.alloc_tab_id();
        let terminal = TerminalModel::new(80, 24);
        terminal.set_wake_channel(Some(self.terminal_wake_tx.clone()));
        self.tabs[parent_index]
            .session
            .as_ref()
            .expect("shell tab has a session")
            .send(SessionCommand::TailFile {
                tail_id: id,
                terminal: terminal.clone(),
                path: path.to_string_lossy().into_owned(),
            });
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let tab = SessionTab {
            id,
            kind: TabKind::Log {
                parent_id: self.tabs[parent_index].id,
                remote_path: path.clone(),
                ended: false,
            },
            profile: None,
            state: ConnState::Connected,
            status: format!("tail -f {}", name),
            session: None,
            terminal,
            focus_handle: cx.focus_handle(),
            tree: Vec::new(),
            root_path: None,
            pending_tree_root: None,
            session_id: 0,
            terminal_focus_pending: true,
            geometry: Arc::new(Mutex::new(None)),
            connecting_profile: None,
            tree_selection: None,
            tree_focus_handle: cx.focus_handle(),
            transfer: None,
            transfer_cancel: None,
            pending_upload_dirs: Vec::new(),
            highlighter: Some(Arc::new(LogHighlighter::load())),
            history_pos: None,
            log_view: Some(LogView::new()),
        };
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.context_menu = None;
        cx.notify();
    }

    // --- REST API (see api.rs) ------------------------------------------------

    /// Index of the connected shell tab for `target` (profile name or
    /// `user@host:port` summary), if any.
    fn connected_tab_for(&self, target: &str) -> Option<usize> {
        self.tabs.iter().position(|tab| {
            !tab.is_log()
                && tab.state == ConnState::Connected
                && tab
                    .profile
                    .as_ref()
                    .is_some_and(|p| p.name == target || p.summary() == target)
        })
    }

    /// Open a visible shell tab for `target`, or use the existing one.
    /// A disconnected tab is reconnected, not left stale.
    fn api_open_session(&mut self, target: &str, cx: &mut Context<Self>) -> Result<(), String> {
        if let Some(index) = self.tabs.iter().position(|tab| {
            !tab.is_log()
                && tab
                    .profile
                    .as_ref()
                    .is_some_and(|p| p.name == target || p.summary() == target)
        }) {
            if self.tabs[index].state == ConnState::Disconnected {
                self.active = index;
                self.reconnect_tab(cx);
            }
            return Ok(());
        }
        let profile = self
            .store
            .profiles
            .iter()
            .find(|p| p.name == target || p.summary() == target)
            .cloned()
            .ok_or_else(|| format!("unknown target '{target}' (see GET /status)"))?;
        if needs_login_prompt(&profile) {
            return Err(format!(
                "profile '{target}' needs credentials — fill them in the UI first"
            ));
        }
        self.spawn_shell_tab(profile, cx);
        Ok(())
    }

    fn api_ok(reply: ApiResponder, value: serde_json::Value) {
        let _ = reply.send(value);
    }

    fn api_err(reply: ApiResponder, message: impl Into<String>) {
        let _ = reply.send(serde_json::json!({"ok": false, "error": message.into()}));
    }

    /// Handle one request forwarded by the HTTP layer. Runs on the UI thread.
// [impl->feat~rest-api~1]
    fn handle_api_request(&mut self, request: ApiRequest, cx: &mut Context<Self>) {
        match request {
            ApiRequest::Status { reply } => {
                let profiles: Vec<serde_json::Value> = self
                    .store
                    .profiles
                    .iter()
                    .map(|profile| {
                        serde_json::json!({
                            "name": profile.name,
                            "summary": profile.summary(),
                        })
                    })
                    .collect();
                let tabs: Vec<serde_json::Value> = self
                    .tabs
                    .iter()
                    .map(|tab| {
                        serde_json::json!({
                            "id": tab.id,
                            "target": tab.profile.as_ref().map(|p| p.name.clone()).unwrap_or_default(),
                            "kind": if tab.is_log() { "log" } else { "shell" },
                            "state": match tab.display_state() {
                                ConnState::Connected => "connected",
                                ConnState::Connecting => "connecting",
                                ConnState::Disconnected => "disconnected",
                            },
                            "status": tab.status,
                        })
                    })
                    .collect();
                Self::api_ok(reply, serde_json::json!({"ok": true, "profiles": profiles, "tabs": tabs}));
            }
            ApiRequest::OpenSession { target, reply } => match self.api_open_session(&target, cx) {
                Ok(()) => Self::api_ok(reply, serde_json::json!({"ok": true})),
                Err(message) => Self::api_err(reply, message),
            },
            ApiRequest::OpenLog { target, path, reply } => {
                if let Some(index) = self.connected_tab_for(&target) {
                    self.activate_tab(index, cx);
                    self.open_log_tab(PathBuf::from(&path), cx);
                    Self::api_ok(reply, serde_json::json!({"ok": true}));
                } else {
                    match self.api_open_session(&target, cx) {
                        Ok(()) => {
                            // The session needs a moment to connect; the poll
                            // loop opens the view once it is up.
                            self.api_pending_log = Some(PendingApiLog {
                                target,
                                path: PathBuf::from(path),
                                reply,
                                deadline: std::time::Instant::now() + Duration::from_secs(30),
                            });
                        }
                        Err(message) => Self::api_err(reply, message),
                    }
                }
            }
            ApiRequest::Exec { target, command, timeout_secs, reply } => {
                let Some(index) = self.connected_tab_for(&target) else {
                    Self::api_err(reply, format!("no connected session for '{target}' — POST /sessions first"));
                    return;
                };
                let req_id = self.next_api_req;
                self.next_api_req += 1;
                self.api_pending.insert(
                    req_id,
                    ApiWait::Exec {
                        reply,
                        deadline: std::time::Instant::now() + Duration::from_secs(timeout_secs + 10),
                    },
                );
                self.tabs[index]
                    .session
                    .as_ref()
                    .expect("shell tab has a session")
                    .send(SessionCommand::ApiExec { req_id, command });
            }
            ApiRequest::ListFiles { target, path, reply } => {
                let Some(index) = self.connected_tab_for(&target) else {
                    Self::api_err(reply, format!("no connected session for '{target}' — POST /sessions first"));
                    return;
                };
                let req_id = self.next_api_req;
                self.next_api_req += 1;
                self.api_pending.insert(
                    req_id,
                    ApiWait::List {
                        reply,
                        deadline: std::time::Instant::now() + Duration::from_secs(70),
                    },
                );
                self.tabs[index]
                    .session
                    .as_ref()
                    .expect("shell tab has a session")
                    .send(SessionCommand::ApiList { req_id, path: PathBuf::from(path) });
            }
            ApiRequest::DownloadFile { target, path, reply } => {
                let Some(index) = self.connected_tab_for(&target) else {
                    Self::api_err(reply, format!("no connected session for '{target}' — POST /sessions first"));
                    return;
                };
                let req_id = self.next_api_req;
                self.next_api_req += 1;
                self.api_pending.insert(
                    req_id,
                    ApiWait::Download {
                        reply,
                        deadline: std::time::Instant::now() + Duration::from_secs(70),
                    },
                );
                self.tabs[index]
                    .session
                    .as_ref()
                    .expect("shell tab has a session")
                    .send(SessionCommand::ApiDownload { req_id, remote: path });
            }
            ApiRequest::UploadFile { target, path, content, reply } => {
                let Some(index) = self.connected_tab_for(&target) else {
                    Self::api_err(reply, format!("no connected session for '{target}' — POST /sessions first"));
                    return;
                };
                // Stage the content into a temp file; the backend streams it.
                let temp_dir = std::env::temp_dir().join("aetherium").join("api-upload");
                let staged = temp_dir.join(format!("{}-{}", uuid::Uuid::new_v4(), path.replace('/', "_")));
                let write = std::fs::create_dir_all(&temp_dir)
                    .and_then(|()| std::fs::write(&staged, &content));
                if let Err(err) = write {
                    Self::api_err(reply, format!("staging upload: {err}"));
                    return;
                }
                let req_id = self.next_api_req;
                self.next_api_req += 1;
                self.api_pending.insert(
                    req_id,
                    ApiWait::Upload {
                        reply,
                        deadline: std::time::Instant::now() + Duration::from_secs(70),
                    },
                );
                self.tabs[index]
                    .session
                    .as_ref()
                    .expect("shell tab has a session")
                    .send(SessionCommand::ApiUpload {
                        req_id,
                        local: staged,
                        remote: path,
                    });
            }
            ApiRequest::CollectLogs { target, reply } => {
                // [impl->feat~rest-log-collection~1]
                if self.api_collect.is_some() {
                    Self::api_err(reply, "a log collection is already running");
                    return;
                }
                if self.connected_tab_for(&target).is_none() {
                    Self::api_err(reply, format!("no connected session for '{target}' — POST /sessions first"));
                    return;
                }
                let mut pending: std::collections::VecDeque<CollectSource> =
                    collect::sources().into();
                let Some(current) = pending.pop_front() else {
                    Self::api_err(reply, "no collect sources configured");
                    return;
                };
                self.api_collect = Some(CollectJob {
                    target,
                    pending,
                    current: Some(current),
                    outcomes: Vec::new(),
                    reply,
                    deadline: std::time::Instant::now()
                        + Duration::from_secs(COLLECT_JOB_TIMEOUT_SECS),
                });
                self.issue_collect_step();
            }
        }
        cx.notify();
    }

    /// Send the current collect source's command to the target session.
    fn issue_collect_step(&mut self) {
        let Some(job) = self.api_collect.as_ref() else {
            return;
        };
        let Some(source) = job.current.as_ref() else {
            return;
        };
        let Some(index) = self.connected_tab_for(&job.target) else {
            let job = self.api_collect.take().expect("checked above");
            Self::api_err(
                job.reply,
                format!("session for '{}' disconnected during collection", job.target),
            );
            return;
        };
        let req_id = self.next_api_req;
        self.next_api_req += 1;
        self.api_pending.insert(
            req_id,
            ApiWait::CollectStep {
                deadline: std::time::Instant::now()
                    + Duration::from_secs(COLLECT_STEP_TIMEOUT_SECS),
            },
        );
        self.tabs[index]
            .session
            .as_ref()
            .expect("shell tab has a session")
            .send(SessionCommand::ApiExec {
                req_id,
                command: source.command(),
            });
    }

    /// Fold one finished source into the collect job and either queue the
    /// next source or answer the waiting HTTP request.
    // [impl->req~configurable-log-sources~1]
    fn collect_step_done(
        &mut self,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        exit_status: Option<u32>,
    ) {
        let Some(mut job) = self.api_collect.take() else {
            return;
        };
        let Some(source) = job.current.take() else {
            self.api_collect = Some(job);
            return;
        };
        let (content, truncated) = if stdout.len() > COLLECT_MAX_BYTES {
            (stdout[..COLLECT_MAX_BYTES].to_vec(), true)
        } else {
            (stdout, false)
        };
        let note = String::from_utf8_lossy(&stderr).trim().to_string();
        // A pipeline like `cat missing | head -c …` exits 0 while cat's
        // complaint lands on stderr — so stderr counts as failure evidence.
        let ok = exit_status.is_none_or(|code| code == 0) && note.is_empty();
        job.outcomes.push(serde_json::json!({
            "name": source.name(),
            "kind": source.kind(),
            "ok": ok,
            "content": String::from_utf8_lossy(&content),
            "bytes": content.len(),
            "truncated": truncated,
            "error": if ok { "" } else { &note[..note.len().min(512)] },
        }));
        if let Some(next) = job.pending.pop_front() {
            job.current = Some(next);
            self.api_collect = Some(job);
            self.issue_collect_step();
        } else {
            Self::api_ok(job.reply, serde_json::json!({"ok": true, "sources": job.outcomes}));
        }
    }

    /// Retry/expiry pass for REST API work, called from the event loop.
    fn api_tick(&mut self, cx: &mut Context<Self>) {
        let now = std::time::Instant::now();
        let expired: Vec<u64> = self
            .api_pending
            .iter()
            .filter(|(_, wait)| wait.deadline() <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            match self.api_pending.remove(&id) {
                // A timed-out collect step fails the whole job; the remote
                // command may keep running, but the reply must not hang.
                Some(ApiWait::CollectStep { .. }) => {
                    if let Some(job) = self.api_collect.take() {
                        Self::api_err(job.reply, "collection step timed out");
                    }
                }
                Some(wait) => wait.fail("request timed out"),
                None => {}
            }
        }
        if let Some(job) = &self.api_collect {
            if now >= job.deadline {
                let job = self.api_collect.take().expect("checked above");
                Self::api_err(job.reply, "collection timed out");
            }
        }
        if let Some(pending) = &self.api_pending_log {
            if now >= pending.deadline {
                let pending = self.api_pending_log.take().expect("checked above");
                Self::api_err(pending.reply, format!("session for '{}' did not connect in time", pending.target));
            } else if let Some(index) = self.connected_tab_for(&pending.target) {
                let pending = self.api_pending_log.take().expect("checked above");
                self.activate_tab(index, cx);
                self.open_log_tab(pending.path, cx);
                Self::api_ok(pending.reply, serde_json::json!({"ok": true}));
            }
        }
    }

    /// Activate a tab, focusing its terminal on the next frame.
    fn activate_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        self.active = index;
        self.context_menu = None;
        self.confirm_delete = None;
        self.tree_editor = None;
        self.tree_drag_target = None;
        self.tree_dragging = None;
        let tab = &mut self.tabs[index];
        tab.terminal_focus_pending = true;
        if let Some(view) = tab.log_view.as_mut() {
            // Results may be stale after time on another tab.
            view.stale = true;
            if view.follow {
                if view.filtering() {
                    view.filter_offset = 0;
                } else {
                    tab.terminal.term.lock().scroll_display(Scroll::Bottom);
                }
            }
        }
        cx.notify();
    }

    /// Toggle following the live edge (SnakeTail's pause/resume). Pausing
    /// keeps the stream buffering in the grid; resuming jumps to the edge.
    fn log_toggle_follow(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        let Some(view) = tab.log_view.as_mut() else {
            return;
        };
        view.follow = !view.follow;
        if view.follow {
            if view.filtering() {
                view.filter_offset = 0;
            } else {
                tab.terminal.term.lock().scroll_display(Scroll::Bottom);
            }
        }
        tab.terminal.mark_dirty();
        cx.notify();
    }

    /// Scroll so the row `line0` lands at the bottom of the viewport,
    /// following when it is already the live edge.
    fn log_reveal_line(&mut self, line0: i32) {
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        let Some(view) = tab.log_view.as_mut() else {
            return;
        };
        if view.filtering() {
            let fed = tab.terminal.lines_fed() as i64;
            if let Some(index) = view
                .filtered
                .iter()
                .position(|anchor| (*anchor - fed) as i32 == line0)
            {
                let len = view.filtered.len();
                view.filter_offset = (len - 1 - index).min(len.saturating_sub(1));
                view.follow = view.filter_offset == 0;
            }
        } else {
            let (min_line0, screen) = tab.terminal.grid_bounds();
            let history = (-min_line0).max(0);
            let d_cur = tab
                .terminal
                .term
                .lock()
                .renderable_content()
                .display_offset as i32;
            let d_target = ((screen - 1) - line0).clamp(0, history);
            if d_target != d_cur {
                tab.terminal
                    .term
                    .lock()
                    .scroll_display(Scroll::Delta(d_target - d_cur));
            }
            view.follow = d_target == 0;
        }
        tab.terminal.mark_dirty();
    }

    /// Step through search matches (SnakeTail's search-and-highlight);
    /// wraps around, scrolls the match into view, and marks it current.
    fn log_search_navigate(&mut self, forward: bool, cx: &mut Context<Self>) {
        let target = {
            let Some(tab) = self.active_tab_mut() else {
                return;
            };
            let Some(view) = tab.log_view.as_mut() else {
                return;
            };
            if view.matches.is_empty() {
                self.status = "no matches".into();
                return;
            }
            let fed = tab.terminal.lines_fed() as i64;
            let len = view.matches.len();
            let next = match view.current_match {
                Some(cur) => {
                    if forward {
                        (cur + 1) % len
                    } else {
                        (cur + len - 1) % len
                    }
                }
                None => {
                    // First match at/after the caret line (before, backwards).
                    let caret0 = view.caret_anchor(&tab.terminal) - fed;
                    view.matches
                        .iter()
                        .position(|(anchor, _)| {
                            let line = *anchor - fed;
                            if forward {
                                line >= caret0
                            } else {
                                line <= caret0
                            }
                        })
                        .unwrap_or(if forward { 0 } else { len - 1 })
                }
            };
            view.current_match = Some(next);
            let anchor = view.matches[next].0;
            view.caret = Some(anchor);
            (anchor - fed) as i32
        };
        self.log_reveal_line(target);
        cx.notify();
    }

    /// Toggle a bookmark on the current line (⌘B).
    fn log_toggle_bookmark(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        let Some(view) = tab.log_view.as_mut() else {
            return;
        };
        let fed = tab.terminal.lines_fed() as i64;
        let (min_line0, screen) = tab.terminal.grid_bounds();
        let anchor = view
            .caret_anchor(&tab.terminal)
            .clamp(fed + min_line0 as i64, fed + screen as i64 - 1);
        if !view.bookmarks.insert(anchor) {
            view.bookmarks.remove(&anchor);
        }
        view.caret = Some(anchor);
        tab.terminal.mark_dirty();
        cx.notify();
    }

    /// Jump between bookmarks (⌘[ / ⌘]).
    fn log_bookmark_navigate(&mut self, forward: bool, cx: &mut Context<Self>) {
        let target = {
            let Some(tab) = self.active_tab_mut() else {
                return;
            };
            let Some(view) = tab.log_view.as_mut() else {
                return;
            };
            if view.bookmarks.is_empty() {
                return;
            }
            let fed = tab.terminal.lines_fed() as i64;
            let caret0 = view.caret_anchor(&tab.terminal) - fed;
            let next = if forward {
                view.bookmarks
                    .iter()
                    .find(|anchor| **anchor - fed > caret0)
                    .or_else(|| view.bookmarks.iter().next())
            } else {
                view.bookmarks
                    .iter()
                    .rev()
                    .find(|anchor| **anchor - fed < caret0)
                    .or_else(|| view.bookmarks.iter().next_back())
            };
            next.map(|anchor| {
                view.caret = Some(*anchor);
                (*anchor - fed) as i32
            })
        };
        if let Some(line0) = target {
            self.log_reveal_line(line0);
        }
        cx.notify();
    }

    /// Close a tab. Log tabs stop their tail channel; closing a shell tab
    /// also closes the log tabs riding on its connection.
    fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let tab_id = tab.id;
        if !tab.is_log() {
            // Close the log children first so their tail channels shut down.
            let children: Vec<u64> = self
                .tabs
                .iter()
                .filter(|child| {
                    matches!(child.kind, TabKind::Log { parent_id, .. } if parent_id == tab_id)
                })
                .map(|child| child.id)
                .collect();
            for child_id in children {
                if let Some(child_index) = self.tab_index_by_id(child_id) {
                    self.close_tab(child_index, cx);
                }
            }
        }
        let tab = &self.tabs[index];
        match &tab.kind {
            TabKind::Log { parent_id, .. } => {
                // Ask the parent connection to stop the tail.
                if let Some(parent) = self.tab_index_by_id(*parent_id) {
                    if let Some(session) = self.tabs[parent].session.as_ref() {
                        session.send(SessionCommand::CloseTail { tail_id: tab_id });
                    }
                }
            }
            TabKind::Shell => {
                if let Some(session) = tab.session.as_ref() {
                    session.disconnect();
                }
            }
        }
        if matches!(self.tabs[index].kind, TabKind::Shell) {
            // The tab (and its event receiver) goes away, so no Disconnected
            // event will arrive to clean up drag-out staging — do it here.
            self.purge_staged_for(self.tabs[index].session_id);
        }
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.active = 0;
        } else if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if index < self.active {
            self.active -= 1;
        }
        self.context_menu = None;
        cx.notify();
    }

    /// Connect from a recent-sessions row: reuse the matching saved profile
    /// when one exists, otherwise open the profile form pre-filled so the
    /// user only has to supply credentials.
    fn connect_recent(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.recents.entries.get(index).cloned() else {
            return;
        };
        if let Some(profile) = self
            .store
            .profiles
            .iter()
            .find(|profile| {
                profile.host == entry.host
                    && profile.username == entry.username
                    && profile.port == entry.port
            })
            .cloned()
        {
            // A double click fires twice; don't start a second connect while
            // one for the same profile is already running.
            if self
                .tabs
                .iter()
                .any(|tab| !tab.is_log() && tab.state == ConnState::Connecting && tab.profile.as_ref().is_some_and(|p| p.name == profile.name))
            {
                return;
            }
            self.connect_profile(profile, window, cx);
        } else {
            self.form = Some(ProfileForm::from_fields(
                cx,
                None,
                AuthKind::Password,
                &entry.profile_name,
                &entry.host,
                &entry.port.to_string(),
                &entry.username,
                "",
                "",
                "",
            ));
            self.focus_first_form_field(window, cx);
            cx.notify();
        }
    }

    fn remove_recent(&mut self, index: usize, cx: &mut Context<Self>) {
        self.recents.remove(index);
        let _ = self.recents.save();
        cx.notify();
    }

    /// Upload OS-dropped files/directories into `remote_dir` via SFTP.
// [impl->feat~os-file-drop-in~1]
    fn upload_dropped_paths(
        &mut self,
        paths: &[PathBuf],
        remote_dir: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let connected = self
            .active_tab()
            .is_some_and(|tab| tab.state == ConnState::Connected);
        if !connected {
            self.status = "connect before dropping files to upload".into();
            cx.notify();
            return;
        }
        if paths.is_empty() {
            return;
        }
        // One flag per batch: cancelling aborts every upload queued here.
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let Some(tab) = self.active_tab() else {
                return;
            };
            let Some(session) = tab.session.as_ref() else {
                return;
            };
            for local in paths {
                session.upload(
                    tab.session_id,
                    local.clone(),
                    remote_dir.clone(),
                    cancel.clone(),
                );
            }
        }
        let tab = self.active_tab_mut().expect("checked above");
        tab.transfer_cancel = Some(cancel);
        if !tab.pending_upload_dirs.contains(&remote_dir) {
            tab.pending_upload_dirs.push(remote_dir);
        }
        cx.notify();
    }

    /// Download the selected tree path of the active tab into ~/Downloads.
    fn download_selected(&mut self, cx: &mut Context<Self>) {
        let connected = self
            .active_tab()
            .is_some_and(|tab| tab.state == ConnState::Connected);
        if !connected {
            self.status = "not connected".into();
            cx.notify();
            return;
        }
        let Some(tab) = self.active_tab() else {
            return;
        };
        let Some(remote) = tab.tree_selection.clone() else {
            self.status = "select a file in the tree to download".into();
            cx.notify();
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        if let Some(session) = tab.session.as_ref() {
            session.download(tab.session_id, remote, cancel.clone());
            if let Some(tab) = self.active_tab_mut() {
                tab.transfer_cancel = Some(cancel);
            }
        }
        cx.notify();
    }

    /// Download the selected directory of the active tab into a single
    /// .zip in ~/Downloads (recursive, with transfer progress).
    // [impl->req~folder-zip-progress~1]
    // [impl->feat~folder-zip-download~1]
    fn download_zip_selected(&mut self, cx: &mut Context<Self>) {
        let connected = self
            .active_tab()
            .is_some_and(|tab| tab.state == ConnState::Connected);
        if !connected {
            self.status = "not connected".into();
            cx.notify();
            return;
        }
        let Some(tab) = self.active_tab() else {
            return;
        };
        let Some(remote) = tab.tree_selection.clone() else {
            self.status = "select a folder in the tree to zip".into();
            cx.notify();
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        if let Some(session) = tab.session.as_ref() {
            session.download_zip(tab.session_id, remote, cancel.clone());
            if let Some(tab) = self.active_tab_mut() {
                tab.transfer_cancel = Some(cancel);
            }
        }
        cx.notify();
    }

    /// Ask before deleting `remote`: deleting is permanent over SFTP (no
    /// trash), so both the context menu and the Delete key end up here
    /// first, at the confirmation dialog.
    fn ask_delete(&mut self, remote: PathBuf, cx: &mut Context<Self>) {
        let (connected, root) = match self.active_tab() {
            Some(tab) => (tab.state == ConnState::Connected, tab.root_path.clone()),
            None => return,
        };
        if !connected {
            self.status = "connect before deleting files".into();
            cx.notify();
            return;
        }
        if root.as_ref() == Some(&remote) {
            if let Some(tab) = self.active_tab_mut() {
                tab.status = "refusing to delete the home directory".into();
            }
            cx.notify();
            return;
        }
        self.context_menu = None;
        self.tree_editor = None;
        self.confirm_delete = Some(remote);
        cx.notify();
    }

    /// Actually delete `remote` (the dialog's Delete button and its Enter
    /// shortcut): dismisses the dialog and sends the backend command.
    fn delete_remote_confirmed(&mut self, remote: PathBuf, cx: &mut Context<Self>) {
        self.confirm_delete = None;
        let (session, session_id) = match self.active_tab() {
            Some(tab) => (tab.session.clone(), tab.session_id),
            None => return,
        };
        let Some(session) = session else {
            self.status = "not connected".into();
            cx.notify();
            return;
        };
        session.delete(session_id, remote.clone());
        if let Some(tab) = self.active_tab_mut() {
            tab.status = format!("deleting {} …", remote.display());
        }
        cx.notify();
    }

    fn on_delete_entry(
        &mut self,
        _: &DeleteEntry,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.confirm_delete.is_some() {
            // The confirmation is already up; don't stack another.
            return;
        }
        let Some(remote) = self.active_tab().and_then(|tab| tab.tree_selection.clone()) else {
            self.status = "select a file in the tree first".into();
            cx.notify();
            return;
        };
        self.ask_delete(remote, cx);
    }

    fn on_cancel_delete(
        &mut self,
        _: &CancelDelete,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Escape: close the inline rename/create editor first; otherwise
        // dismiss the delete confirmation.
        if self.tree_editor.is_some() {
            self.cancel_tree_editor(window, cx);
            return;
        }
        if self.confirm_delete.take().is_some() {
            cx.notify();
        }
    }

    fn on_confirm_delete(
        &mut self,
        _: &ConfirmDelete,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Enter: submit the inline rename/create editor when one is open;
        // otherwise confirm the delete dialog.
        if self.tree_editor.is_some() {
            self.submit_tree_editor(window, cx);
            return;
        }
        if let Some(remote) = self.confirm_delete.clone() {
            self.delete_remote_confirmed(remote, cx);
        }
    }

    /// Focus the active tab's file tree (row clicks do this so the Delete
    /// key dispatches to the tree rather than whatever had focus before).
    fn focus_file_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(handle) = self.active_tab().map(|tab| tab.tree_focus_handle.clone()) {
            window.focus(&handle, cx);
        }
    }

    /// Spawn a fresh backend for the active tab and connect it again with
    /// its original profile. Phone sessions drop constantly; this is the
    /// one-click way back.
    fn reconnect_tab(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        if tab.is_log() || tab.state != ConnState::Disconnected {
            return;
        }
        let Some(profile) = tab.profile.clone() else {
            return;
        };
        let old_session_id = tab.session_id;
        let terminal = tab.terminal.clone();
        terminal.reset();
        let session = SessionHandle::spawn(terminal);
        session.connect(profile.clone());
        let tab = self.active_tab_mut().expect("checked above");
        tab.session = Some(session);
        tab.session_id += 1;
        tab.state = ConnState::Connecting;
        tab.status = format!("reconnecting to {}…", profile.summary());
        tab.tree.clear();
        tab.root_path = None;
        tab.tree_selection = None;
        tab.transfer = None;
        tab.transfer_cancel = None;
        tab.terminal_focus_pending = true;
        // Drag-out staging belonged to the dead session.
        self.purge_staged_for(old_session_id);
        let label = Self::tab_log_label(&self.tabs[self.active]);
        self.log(LogLevel::Info, format!("{label}: reconnecting"));
        cx.notify();
    }

    /// Switch the active theme (from the header's theme menu). Persists the
    /// choice; the whole UI re-reads colors on the next paint.
// [impl->feat~theme-system~1]
    fn apply_theme(&mut self, name: String, cx: &mut Context<Self>) {
        if theme::set_active(&name) {
            self.theme_menu = false;
            self.status = format!("theme: {name}");
        } else {
            self.status = format!("unknown theme: {name}");
        }
        cx.notify();
    }

    /// Open the inline editor renaming `path`.
    fn start_rename(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.tree_editor.is_some() {
            return;
        }
        let (parent, name) = match self.active_tab() {
            Some(tab) => {
                if tab.state != ConnState::Connected {
                    self.status = "connect before renaming files".into();
                    cx.notify();
                    return;
                }
                if tab.root_path.as_ref() == Some(&path) {
                    if let Some(tab) = self.active_tab_mut() {
                        tab.status = "cannot rename the home directory".into();
                    }
                    cx.notify();
                    return;
                }
                let parent = match path.parent().map(PathBuf::from) {
                    Some(parent) => parent,
                    None => return,
                };
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (parent, name)
            }
            None => return,
        };
        let field = cx.new(|cx| {
            let mut field = TextField::new(cx, "name");
            field.set_text(name, cx);
            field
        });
        self.tree_editor = Some(TreeEditor {
            target: Some(path),
            parent,
            is_dir: false,
            field: field.clone(),
        });
        window.focus(&field.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    /// Open the inline editor creating a new file (`is_dir = false`) or
    /// folder inside the selected directory — or the home directory when
    /// nothing is selected.
    fn start_create(&mut self, is_dir: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.tree_editor.is_some() {
            return;
        }
        let Some(tab) = self.active_tab() else {
            return;
        };
        if tab.state != ConnState::Connected {
            self.status = "connect before creating files".into();
            cx.notify();
            return;
        }
        // Parent: the selected directory, the selected file's directory, or
        // the home directory.
        let (root, selection) = (tab.root_path.clone(), tab.tree_selection.clone());
        let tab_mut = self.active_tab_mut().expect("checked above");
        let parent = match selection {
            Some(path) => {
                let is_dir = find_node(&mut tab_mut.tree, &path)
                    .map(|node| node.entry.is_dir)
                    .unwrap_or(false);
                if is_dir {
                    path
                } else {
                    path.parent().map(PathBuf::from).unwrap_or(path)
                }
            }
            None => match root {
                Some(root) => root,
                None => return,
            },
        };
        // Show the editor row: the parent must be expanded with its
        // children loaded (or loading) on screen.
        let session = tab_mut.session.clone();
        let session_id = tab_mut.session_id;
        if let Some(node) = find_node(&mut tab_mut.tree, &parent) {
            node.expanded = true;
            if node.children.is_none() {
                node.loading = true;
                if let Some(session) = session {
                    session.list_dir(session_id, parent.clone());
                }
            }
        }
        let placeholder = if is_dir { "new folder" } else { "new file" };
        let field = cx.new(|cx| TextField::new(cx, placeholder));
        self.tree_editor = Some(TreeEditor {
            target: None,
            parent,
            is_dir,
            field: field.clone(),
        });
        window.focus(&field.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    /// Enter in the inline editor: validate the name and send the backend
    /// command (rename or create).
    fn submit_tree_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.tree_editor.take() else {
            return;
        };
        // Enter can also arrive through the IME text path; flatten it away.
        let name = editor
            .field
            .read(cx)
            .text()
            .trim()
            .replace(['\n', '\r'], "");
        self.focus_file_tree(window, cx);
        let Some(tab) = self.active_tab() else {
            return;
        };
        let Some(session) = tab.session.clone() else {
            return;
        };
        let session_id = tab.session_id;
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            if let Some(tab) = self.active_tab_mut() {
                tab.status = "invalid name".into();
            }
            cx.notify();
            return;
        }
        match &editor.target {
            Some(from) => {
                let to = editor.parent.join(&name);
                if to == *from {
                    return; // unchanged: nothing to do
                }
                let from = from.clone();
                session.rename(session_id, from.clone(), to.clone());
                if let Some(tab) = self.active_tab_mut() {
                    tab.status = format!("renaming {} → {} …", from.display(), to.display());
                }
            }
            None => {
                let path = editor.parent.join(&name);
                if editor.is_dir {
                    session.create_dir(session_id, path.clone());
                } else {
                    session.create_file(session_id, path.clone());
                }
                if let Some(tab) = self.active_tab_mut() {
                    tab.status = format!("creating {} …", path.display());
                }
            }
        }
        cx.notify();
    }

    /// Escape in the inline editor: dismiss without doing anything.
    fn cancel_tree_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tree_editor.take().is_some() {
            self.focus_file_tree(window, cx);
            cx.notify();
        }
    }

    fn on_rename_entry(
        &mut self,
        _: &RenameEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tree_editor.is_some() {
            return;
        }
        let Some(remote) = self.active_tab().and_then(|tab| tab.tree_selection.clone()) else {
            self.status = "select a file in the tree first".into();
            cx.notify();
            return;
        };
        self.start_rename(remote, window, cx);
    }

    /// Stage a remote file locally and open the temp copy in a local
    /// editor (MobaXterm-style remote editing). The temp copy is watched;
    /// every save pops a "sync back to the device?" dialog.
// [impl->feat~remote-edit-writeback~1]
    fn start_remote_edit(&mut self, remote: PathBuf, editor: EditorChoice, cx: &mut Context<Self>) {
        self.context_menu = None;
        let connected = self
            .active_tab()
            .is_some_and(|tab| tab.state == ConnState::Connected);
        if !connected {
            self.status = "not connected".into();
            cx.notify();
            return;
        }
        let Some(tab) = self.active_tab() else {
            return;
        };
        let session = tab.session.clone();
        let session_id = tab.session_id;
        let target = tab
            .profile
            .as_ref()
            .map(|profile| profile.summary())
            .unwrap_or_default();
        // Already staged (or waiting): just (re)open the editor.
        if let Some(edit) = self
            .remote_edits
            .iter()
            .find(|edit| edit.session_id == session_id && edit.remote == remote)
        {
            let local = edit.local.clone();
            if Self::launch_editor(editor, &local).is_ok() {
                self.status = format!("editing {} — saves ask to sync back", remote.display());
            } else {
                self.status = "could not launch the editor".into();
            }
            cx.notify();
            return;
        }
        let pending = PendingEditorOpen {
            session_id,
            remote: remote.clone(),
            editor,
        };
        if !self
            .pending_editor_open
            .iter()
            .any(|waiting| {
                waiting.session_id == pending.session_id
                    && waiting.remote == pending.remote
                    && waiting.editor == pending.editor
            })
        {
            self.pending_editor_open.push(pending);
        }
        // A previous staging (drag-out or an earlier edit) is reused.
        let key = (session_id, remote.clone());
        let staged = self.temp_download_cache.lock().get(&key).cloned();
        match (staged, session) {
            (Some(local), _) => {
                self.begin_remote_edit(session_id, remote, local, target, editor, cx)
            }
            (None, Some(session)) => {
                log::info!("edit: staging {}", remote.display());
                session.download_to_temp(session_id, remote.clone(), self.temp_download_cache.clone());
                self.status = format!("staging {}…", remote.display());
            }
            (None, None) => self.status = "not connected".into(),
        }
        cx.notify();
    }

    /// Register a staged temp file as a watched remote edit and open it.
    fn begin_remote_edit(
        &mut self,
        session_id: u64,
        remote: PathBuf,
        local: PathBuf,
        target: String,
        editor: EditorChoice,
        cx: &mut Context<Self>,
    ) {
        let (mtime, size) = std::fs::metadata(&local)
            .map(|metadata| {
                (
                    metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
                    metadata.len(),
                )
            })
            .unwrap_or((std::time::UNIX_EPOCH, 0));
        self.remote_edits.push(RemoteEdit {
            session_id,
            remote: remote.clone(),
            local: local.clone(),
            target,
            mtime,
            size,
        });
        match Self::launch_editor(editor, &local) {
            Ok(()) => {
                self.status = format!("editing {} — saves ask to sync back", remote.display());
            }
            Err(err) => {
                self.status = format!("editor: {err:#}");
            }
        }
        cx.notify();
    }

    /// Launch a local editor on a staged file.
    fn launch_editor(choice: EditorChoice, local: &std::path::Path) -> anyhow::Result<()> {
        match choice {
            EditorChoice::VsCode => {
                #[cfg(target_os = "macos")]
                let result = std::process::Command::new("open")
                    .args(["-a", "Visual Studio Code"])
                    .arg(local)
                    .spawn();
                #[cfg(not(target_os = "macos"))]
                let result = std::process::Command::new("code").arg(local).spawn();
                result.map(|_| ()).map_err(|err| err.into())
            }
            EditorChoice::Default => {
                // $AETHERIUM_EDITOR wins ("code --wait", "subl", …); otherwise
                // the OS default handler for the file type.
                let spec = std::env::var("AETHERIUM_EDITOR").ok();
                #[cfg(target_os = "macos")]
                let fallback = "open";
                #[cfg(target_os = "linux")]
                let fallback = "xdg-open";
                #[cfg(windows)]
                let fallback = "explorer";
                let spec = spec.unwrap_or_else(|| fallback.to_string());
                let mut parts = spec.split_whitespace();
                let program = parts.next().unwrap_or(fallback);
                let mut command = std::process::Command::new(program);
                command.args(parts).arg(local);
                command.spawn().map(|_| ()).map_err(|err| err.into())
            }
        }
    }

    /// Poll watched edits; a modified temp copy asks to sync back (one
    /// question at a time, like MobaXterm).
    fn edit_scan(&mut self, cx: &mut Context<Self>) {
        if self.last_edit_scan.elapsed() < Duration::from_millis(700) {
            return;
        }
        self.last_edit_scan = std::time::Instant::now();
        if self.edit_sync_ask.is_some() {
            return;
        }
        for index in 0..self.remote_edits.len() {
            let edit = &self.remote_edits[index];
            let Ok(metadata) = std::fs::metadata(&edit.local) else {
                continue;
            };
            let mtime = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
            if mtime != edit.mtime || metadata.len() != edit.size {
                self.edit_sync_ask = Some(EditSyncAsk {
                    session_id: edit.session_id,
                    remote: edit.remote.clone(),
                    local: edit.local.clone(),
                    target: edit.target.clone(),
                });
                cx.notify();
                return;
            }
        }
    }

    /// Answer the sync dialog: `true` uploads back, `false` just resets the
    /// watch baseline.
    fn answer_edit_sync(&mut self, upload: bool, cx: &mut Context<Self>) {
        let Some(ask) = self.edit_sync_ask.take() else {
            return;
        };
        // Reset the baseline either way (to the file's current state).
        if let Some(edit) = self
            .remote_edits
            .iter_mut()
            .find(|edit| edit.session_id == ask.session_id && edit.remote == ask.remote)
        {
            if let Ok(metadata) = std::fs::metadata(&ask.local) {
                edit.mtime = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
                edit.size = metadata.len();
            }
        }
        if upload {
            // Upload into the remote file's parent directory, keeping the
            // file name (the shared transfer UI shows progress).
            let remote_dir = ask
                .remote
                .parent()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            if let Some(tab) = self.tabs.iter().find(|tab| {
                !tab.is_log()
                    && tab.session_id == ask.session_id
                    && tab.state == ConnState::Connected
            }) {
                if let Some(session) = tab.session.as_ref() {
                    session.send(SessionCommand::Upload {
                        session_id: ask.session_id,
                        local: ask.local.clone(),
                        remote_dir: remote_dir.clone(),
                        cancel: Arc::new(AtomicBool::new(false)),
                    });
                    self.status = format!("uploading {}…", ask.remote.display());
                }
            } else {
                self.status = "session for that file is gone — sync skipped".into();
            }
        }
        cx.notify();
    }

    /// Disconnect the active shell tab (the tab itself stays open).
    fn disconnect(&mut self, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_tab() {
            if let Some(session) = tab.session.as_ref() {
                session.disconnect();
            }
        }
        cx.notify();
    }

    fn open_new_profile_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.form = Some(ProfileForm::blank(cx));
        self.focus_first_form_field(window, cx);
        cx.notify();
    }

    fn open_edit_profile_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.selected else {
            self.status = "select a profile to edit".into();
            cx.notify();
            return;
        };
        if let Some(profile) = self.store.profiles.get(index).cloned() {
            self.form = Some(ProfileForm::from_profile(Some(index), &profile, cx));
            self.focus_first_form_field(window, cx);
            cx.notify();
        }
    }

    /// Focus the first field of the freshly opened profile form.
    fn focus_first_form_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(form) = self.form.as_ref() {
            let handle = form.name.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
    }

    fn delete_selected_profile(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.selected {
            self.store.remove(index);
            let _ = self.store.save();
            self.selected = None;
            cx.notify();
        }
    }

    fn save_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.form.take() else { return };
        let auth = match form.auth_kind {
            AuthKind::Password => AuthMethod::Password {
                password: form.password.read(cx).text().to_string(),
            },
            AuthKind::KeyFile => AuthMethod::KeyFile {
                path: PathBuf::from(form.key_path.read(cx).text()),
                passphrase: if form.passphrase.read(cx).is_empty() {
                    None
                } else {
                    Some(form.passphrase.read(cx).text().to_string())
                },
            },
            AuthKind::Agent => AuthMethod::Agent,
        };
        let port = form.port.read(cx).text().trim().parse().unwrap_or(22);
        let name = form.name.read(cx).text().trim().to_string();
        let host = form.host.read(cx).text().trim().to_string();
        let username = form.username.read(cx).text().trim().to_string();
        let profile = Profile {
            name: if name.is_empty() {
                format!("{username}@{host}")
            } else {
                name
            },
            host,
            port,
            username,
            auth,
        };
        let editing = form.editing;
        self.store.upsert(editing, profile);
        match self.store.save() {
            Ok(()) => self.status = format!("profiles saved to {}", self.store.path().display()),
            Err(err) => self.status = format!("failed to save profiles: {err:#}"),
        }
        self.selected = Some(editing.unwrap_or(self.store.profiles.len() - 1));
        cx.notify();
    }

    fn cancel_form(&mut self, cx: &mut Context<Self>) {
        self.form = None;
        cx.notify();
    }

    /// Move focus to the next/previous text field of the open profile form
    /// (Tab / Shift+Tab), following the visual row order and wrapping around
    /// at both ends.
    fn form_tab(&mut self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let mut fields = vec![
            form.name.clone(),
            form.host.clone(),
            form.port.clone(),
            form.username.clone(),
        ];
        match form.auth_kind {
            AuthKind::Password => fields.push(form.password.clone()),
            AuthKind::KeyFile => {
                fields.push(form.key_path.clone());
                fields.push(form.passphrase.clone());
            }
            AuthKind::Agent => {}
        }
        let current = fields
            .iter()
            .position(|field| field.focus_handle(cx).is_focused(window));
        let next = match current {
            Some(index) if backwards => (index + fields.len() - 1) % fields.len(),
            Some(index) => (index + 1) % fields.len(),
            None => 0,
        };
        let focus_handle = fields[next].focus_handle(cx);
        window.focus(&focus_handle, cx);
    }

    fn on_terminal_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Font zoom: cmd +/- / cmd 0 (like Zed's terminal). On Windows/Linux
        // gpui's `platform` modifier is the Windows key, so ctrl takes over
        // there — Ctrl+wheel zooms too. Handled before the tab borrow; these
        // keys never reach the PTY.
        // [impl->req~font-zoom-shortcuts~1]
        let zoom_mods = &event.keystroke.modifiers;
        #[cfg(target_os = "macos")]
        let zoom_pressed = zoom_mods.platform && !zoom_mods.control && !zoom_mods.alt;
        #[cfg(not(target_os = "macos"))]
        let zoom_pressed = !zoom_mods.alt && (zoom_mods.platform || zoom_mods.control);
        if zoom_pressed {
            match event.keystroke.key.as_str() {
                "=" | "+" => {
                    self.zoom_terminal(1.0, cx);
                    return;
                }
                "-" | "_" => {
                    self.zoom_terminal(-1.0, cx);
                    return;
                }
                "0" => {
                    self.reset_terminal_font(cx);
                    return;
                }
                _ => {}
            }
        }
        let Some(tab) = self.active_tab() else {
            return;
        };
        let terminal = tab.terminal.clone();
        let is_log = tab.is_log();
        let tab_index = self.active;
        let session_handle = tab.session.clone();
        let keystroke = &event.keystroke;
        let mods = &keystroke.modifiers;

        // Scrollback navigation and history recall do not go to the PTY.
        if mods.shift && !mods.control && !mods.alt {
            let scroll = match keystroke.key.as_str() {
                "pageup" => Some(Scroll::PageUp),
                "pagedown" => Some(Scroll::PageDown),
                _ => None,
            };
            if let Some(scroll) = scroll {
                terminal.term.lock().scroll_display(scroll);
                terminal.mark_dirty();
                cx.notify();
                return;
            }
            // Shift+↑/↓ recall the cross-session command history. Not in
            // full-screen apps, where these keys belong to the app.
            if matches!(keystroke.key.as_str(), "up" | "down")
                && !terminal.term.lock().mode().contains(TermMode::ALT_SCREEN)
            {
                self.recall_history(keystroke.key == "up", cx);
                return;
            }
        }

        // Copy: cmd-c (macOS) / ctrl-shift-c / ctrl+insert everywhere; on
        // Windows also plain Ctrl+C while a selection exists (Windows
        // Terminal semantics). The keystroke never reaches the PTY.
        // Placed before the read-only check so log tabs, whose whole
        // point is reading, are copyable too.
        #[cfg(windows)]
        let has_selection = terminal.has_selection();
        #[cfg(windows)]
        let copy = keystroke.key == "c"
            && (mods.platform
                || (mods.control && mods.shift && !mods.alt)
                || (mods.control && !mods.alt && has_selection));
        #[cfg(not(windows))]
        let copy = keystroke.key == "c" && ((mods.control && mods.shift && !mods.alt) || mods.platform);
        let copy_insert = keystroke.key == "insert" && mods.control && !mods.alt && !mods.shift;
        if copy || copy_insert {
            if let Some(text) = terminal.selected_text() {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                let len = text.len() as u64;
                if let Some(tab) = self.active_tab_mut() {
                    tab.status = format!("copied {}", format_size(len));
                }
            }
            cx.notify();
            return;
        }

        // Log-follow tabs are read-only views of `tail -f` output.
        if is_log {
            let focused = window.focused(cx);
            let (search_focus, filter_focus) = self
                .active_tab()
                .and_then(|tab| tab.log_view.as_ref())
                .map(|view| {
                    (
                        view.search_field
                            .as_ref()
                            .map(|field| field.read(cx).focus_handle(cx)),
                        view.filter_field
                            .as_ref()
                            .map(|field| field.read(cx).focus_handle(cx)),
                    )
                })
                .unwrap_or((None, None));
            let field_focused = search_focus
                .as_ref()
                .is_some_and(|handle| Some(handle) == focused.as_ref())
                || filter_focus
                    .as_ref()
                    .is_some_and(|handle| Some(handle) == focused.as_ref());

            if mods.platform && !mods.control && !mods.alt {
                match keystroke.key.as_str() {
                    "f" => {
                        if let Some(handle) = search_focus {
                            window.focus(&handle, cx);
                        }
                        return;
                    }
                    "g" => {
                        self.log_search_navigate(!mods.shift, cx);
                        return;
                    }
                    "b" => {
                        self.log_toggle_bookmark(cx);
                        return;
                    }
                    "[" => {
                        self.log_bookmark_navigate(false, cx);
                        return;
                    }
                    "]" => {
                        self.log_bookmark_navigate(true, cx);
                        return;
                    }
                    _ => {}
                }
            }
            if field_focused {
                match keystroke.key.as_str() {
                    // Enter in the search field steps matches; in the filter
                    // field it just returns focus to the log (the poll loop
                    // applies filter text as it changes).
                    "enter" => {
                        if search_focus
                            .as_ref()
                            .is_some_and(|handle| Some(handle) == focused.as_ref())
                        {
                            self.log_search_navigate(!mods.shift, cx);
                        } else if let Some(tab) = self.active_tab() {
                            window.focus(&tab.focus_handle, cx);
                        }
                        return;
                    }
                    "escape" => {
                        if let Some(tab) = self.active_tab() {
                            window.focus(&tab.focus_handle, cx);
                        }
                        return;
                    }
                    _ => {}
                }
            }
            // Log scrolling keys.
            if !mods.platform && !mods.control && !mods.alt {
                let scroll = match keystroke.key.as_str() {
                    "up" => Some(Scroll::Delta(1)),
                    "down" => Some(Scroll::Delta(-1)),
                    "pageup" => Some(Scroll::PageUp),
                    "pagedown" => Some(Scroll::PageDown),
                    "home" => Some(Scroll::Top),
                    "end" => Some(Scroll::Bottom),
                    _ => None,
                };
                if let Some(scroll) = scroll {
                    self.log_scroll_active(scroll);
                    cx.notify();
                    return;
                }
            }
            return;
        }

        // Paste: cmd-v (macOS) / ctrl-shift-v everywhere; on Windows also
        // plain Ctrl+V and shift+insert (Windows Terminal / conhost
        // conventions). ctrl-v without shift still reaches the PTY on
        // other platforms (quoted-insert for readline/emacs users).
        #[cfg(windows)]
        let paste = (keystroke.key == "v" && mods.control && !mods.alt)
            || (keystroke.key == "insert" && mods.shift && !mods.control && !mods.alt);
        #[cfg(not(windows))]
        let paste = (keystroke.key == "v" && ((mods.control && mods.shift && !mods.alt) || mods.platform))
            || (keystroke.key == "insert" && mods.shift && !mods.control && !mods.alt);
        if paste {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                let mode = *terminal.term.lock().mode();
                let bytes = if mode.contains(TermMode::BRACKETED_PASTE) {
                    // Wrap the paste so the receiving program can tell it
                    // apart from typed input.
                    let mut bytes = Vec::with_capacity(text.len() + 12);
                    bytes.extend_from_slice(b"\x1b[200~");
                    bytes.extend_from_slice(text.as_bytes());
                    bytes.extend_from_slice(b"\x1b[201~");
                    bytes
                } else {
                    text.into_bytes()
                };
                if let Some(session) = session_handle.as_ref() {
                    self.track_command_input(tab_index, &bytes);
                    session.input(bytes);
                }
                self.scroll_to_bottom();
            }
            cx.notify();
            return;
        }
        let _ = window;

        let mode = *terminal.term.lock().mode();
        if let Some(bytes) = TerminalModel::keystroke_to_bytes(keystroke, mode) {
            // Instant local echo on the primary screen (full-screen apps
            // don't echo input); the server's identical echo is dropped on
            // arrival. Echo before sending so the dedupe is always armed in
            // time, even on fast links.
            if self.local_echo && !mode.contains(TermMode::ALT_SCREEN) {
                terminal.echo_input(&bytes);
            }
            if let Some(session) = session_handle.as_ref() {
                self.track_command_input(tab_index, &bytes);
                session.input(bytes);
            }
            self.scroll_to_bottom();
        }
        cx.notify();
    }

    /// Apply a scroll action to the active log tab — the grid's display
    /// offset, or the filter window's row offset — and update the follow
    /// state. Returns `true` when the scroll landed on the live edge.
    fn log_scroll_active(&mut self, scroll: Scroll) -> bool {
        let Some(tab) = self.active_tab_mut() else {
            return false;
        };
        let Some(view) = tab.log_view.as_mut() else {
            return false;
        };
        if view.filtering() {
            // Bottom-anchored window: a bigger offset shows older rows.
            let len = view.filtered.len() as i32;
            let delta = match scroll {
                Scroll::Delta(n) => n,
                Scroll::PageUp => 10,
                Scroll::PageDown => -10,
                Scroll::Top => i32::MAX,
                Scroll::Bottom => 0,
            };
            let next = if delta == i32::MAX {
                i32::MAX
            } else {
                view.filter_offset as i32 + delta
            };
            view.filter_offset = next.clamp(0, (len - 1).max(0)) as usize;
            view.follow = view.filter_offset == 0;
            tab.terminal.mark_dirty();
            view.follow
        } else {
            tab.terminal.term.lock().scroll_display(scroll);
            let at_bottom = matches!(scroll, Scroll::Bottom)
                || tab
                    .terminal
                    .term
                    .lock()
                    .renderable_content()
                    .display_offset
                    == 0;
            view.follow = at_bottom;
            tab.terminal.mark_dirty();
            at_bottom
        }
    }

    /// Jump the display back to the live edge (used whenever input is sent).
    fn scroll_to_bottom(&self) {
        if let Some(tab) = self.active_tab() {
            tab.terminal.term.lock().scroll_display(Scroll::Bottom);
            tab.terminal.mark_dirty();
        }
    }

    /// Abort the active transfer of the active tab. The backend task checks
    /// the flag between chunks and removes the partial file.
    fn cancel_transfer(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        if let Some(cancel) = tab.transfer_cancel.as_ref() {
            cancel.store(true, Ordering::Relaxed);
            tab.status = "cancelling transfer…".into();
            cx.notify();
        }
    }

    /// Feed outgoing bytes into the input-line tracker; a submitted command
    /// joins the cross-session history (persisted immediately). Any normal
    /// typing leaves the history-recall position.
    fn track_command_input(&mut self, tab_index: usize, bytes: &[u8]) {
        let Some(tab) = self.tabs.get_mut(tab_index) else {
            return;
        };
        tab.history_pos = None;
        if let Some(command) = tab.terminal.track_input(bytes) {
            self.history.push(command);
            if let Err(err) = self.history.save() {
                self.log(LogLevel::Warn, format!("failed to save command history: {err:#}"));
            }
        }
    }

    /// Shift+↑/↓: recall the previous/next command from the cross-session
    /// history. The recalled text replaces the input line: ^U asks the
    /// remote line editor to clear it, then the command goes in as if
    /// typed.
    fn recall_history(&mut self, backwards: bool, cx: &mut Context<Self>) {
        let tab_index = self.active;
        let Some(tab) = self.tabs.get(tab_index) else {
            return;
        };
        if tab.is_log() || self.history.is_empty() {
            return;
        }
        let history_len = self.history.len();
        // Position within the history; the history length doubles as the
        // live, not-yet-submitted line.
        let pos = tab.history_pos.unwrap_or(history_len);
        let new_pos = if backwards {
            pos.saturating_sub(1)
        } else {
            (pos + 1).min(history_len)
        };
        if new_pos == pos {
            return; // oldest entry, or already back at the live line
        }
        self.tabs[tab_index].history_pos = Some(new_pos);
        let command = if new_pos == history_len {
            String::new() // back at the live line: clear the recalled text
        } else {
            self.history.commands()[new_pos].clone()
        };
        let terminal = self.tabs[tab_index].terminal.clone();
        let session = self.tabs[tab_index].session.clone();
        terminal.set_input_line(&command);
        let mut bytes = vec![0x15u8]; // ^U: kill the remote line before inserting.
        bytes.extend_from_slice(command.as_bytes());
        if self.local_echo && !command.is_empty() {
            terminal.echo_input(command.as_bytes());
        }
        if let Some(session) = session {
            session.input(bytes);
        }
        if let Some(tab) = self.tabs.get_mut(tab_index) {
            tab.status = if command.is_empty() {
                "history: back to the current line".into()
            } else {
                format!("history: {command}")
            };
        }
        self.scroll_to_bottom();
        cx.notify();
    }

    /// Zoom the terminal font by `delta` pixels (cmd/ctrl +/-, cmd/ctrl+wheel),
    /// clamped to a sane range. The grid re-measures on the next frame and the
    /// PTY resize follows via the repaint loop's geometry check; the size
    /// persists across launches.
    // [impl->req~font-zoom-shortcuts~1]
    fn zoom_terminal(&mut self, delta: f32, cx: &mut Context<Self>) {
        let new_size = (self.terminal_font_size + delta).clamp(8.0, 32.0);
        if (new_size - self.terminal_font_size).abs() < f32::EPSILON {
            return;
        }
        self.terminal_font_size = new_size;
        self.save_ui_settings();
        for tab in &self.tabs {
            tab.terminal.mark_dirty();
        }
        cx.notify();
    }

    /// Back to the default terminal font size (cmd/ctrl 0).
    // [impl->req~font-zoom-shortcuts~1]
    // [impl->feat~terminal-font-zoom~1]
    fn reset_terminal_font(&mut self, cx: &mut Context<Self>) {
        if (self.terminal_font_size - TERMINAL_FONT_SIZE).abs() < f32::EPSILON {
            return;
        }
        self.terminal_font_size = TERMINAL_FONT_SIZE;
        self.save_ui_settings();
        for tab in &self.tabs {
            tab.terminal.mark_dirty();
        }
        cx.notify();
    }

    fn save_ui_settings(&self) {
        UiSettings {
            terminal_font_size: self.terminal_font_size,
            shell_coloring: self.shell_coloring,
        }
        .save();
    }

    fn toggle_tree_node(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        tab.tree_selection = Some(path.clone());
        let needs_load = if let Some(node) = find_node(&mut tab.tree, &path) {
            if !node.entry.is_dir {
                tab.status = match node.entry.modified {
                    Some(mtime) => format!(
                        "{} ({}, modified @{mtime})",
                        path.display(),
                        format_size(node.entry.size)
                    ),
                    None => format!("{} ({})", path.display(), format_size(node.entry.size)),
                };
                cx.notify();
                return;
            }
            if node.expanded {
                node.expanded = false;
                false
            } else if node.children.is_some() {
                node.expanded = true;
                false
            } else {
                node.loading = true;
                true
            }
        } else {
            false
        };
        if needs_load {
            let session_id = tab.session_id;
            if let Some(session) = tab.session.as_ref() {
                session.list_dir(session_id, path);
            }
        }
        cx.notify();
    }

    /// Reset internal file-tree drag state (called on drop and when a drag
    /// ends without one).
    fn clear_tree_drag(&mut self, cx: &mut Context<Self>) {
        if self.tree_drag_target.take().is_some() || self.tree_dragging.take().is_some() {
            cx.notify();
        }
    }

    /// Delete all drag-out staging files cached for `session_id` (the uuid
    /// temp dirs) and drop their cache entries.
    fn purge_staged_for(&self, session_id: u64) {
        self.temp_download_cache.lock().retain(|(sid, _), local| {
            if *sid == session_id {
                if let Some(dir) = local.parent() {
                    let _ = std::fs::remove_dir_all(dir);
                }
                false
            } else {
                true
            }
        });
    }

    /// Short label identifying a tab in log messages.
    fn tab_log_label(tab: &SessionTab) -> String {
        if let Some(profile) = tab.profile.as_ref() {
            return profile.summary();
        }
        match &tab.kind {
            TabKind::Log { remote_path, .. } => format!("tail {}", remote_path.display()),
            TabKind::Shell => "session".to_string(),
        }
    }

    /// Append a line to the tool log (the Logs sidebar tab). Connection
    /// issues, transfer errors, move failures, etc. all land here.
    fn log(&mut self, level: LogLevel, message: impl Into<String>) {
        let time = chrono::Local::now().format("%H:%M:%S").to_string();
        self.logs.push(LogEntry {
            time,
            level,
            message: message.into(),
        });
        if self.logs.len() > MAX_LOG_ENTRIES {
            let excess = self.logs.len() - MAX_LOG_ENTRIES;
            self.logs.drain(0..excess);
        }
        self.logs_scroll_pending = true;
    }

    /// Directory to highlight while dragging, mirroring Zed's
    /// `highlight_entry_for_selection_drag`: directories highlight
    /// themselves, files highlight their parent, and hovering the entry's
    /// own parent (or its sibling files) highlights nothing. The background
    /// highlights the tree root unless the entry already lives there.
    fn tree_drop_highlight(&self, dragged: &DraggedEntry, target: &TreeDragTarget) -> Option<PathBuf> {
        let root = self.active_tab()?.root_path.clone()?;
        let (hover, hover_is_dir) = match target {
            TreeDragTarget::Background => {
                // Reject the same cases the drop itself rejects.
                let into_own_subtree =
                    dragged.is_dir && root.starts_with(dragged.path.as_path());
                let already_at_root = dragged.path.parent() == Some(root.as_path());
                return (!into_own_subtree && !already_at_root).then_some(root);
            }
            TreeDragTarget::Row { path, is_dir } => (path.clone(), *is_dir),
        };
        // Don't advertise drops the move would reject: a directory can't be
        // dropped onto itself or into its own subtree.
        if dragged.is_dir && hover.starts_with(dragged.path.as_path()) {
            return None;
        }
        let dragged_parent = dragged.path.parent();
        if dragged_parent == Some(hover.as_path()) {
            return None;
        }
        if !hover_is_dir && dragged_parent.is_some() && dragged_parent == hover.parent() {
            return None;
        }
        if hover_is_dir {
            Some(hover)
        } else {
            hover.parent().map(PathBuf::from)
        }
    }

    /// Move a dragged entry via SFTP rename. `target` is the row (or tree
    /// background) the entry was dropped on; directories drop into
    /// themselves, files drop into their parent directory.
    fn drop_tree_entry(
        &mut self,
        dragged: DraggedEntry,
        target: TreeDragTarget,
        cx: &mut Context<Self>,
    ) {
        self.clear_tree_drag(cx);
        // The drag ended inside the app, so the staged temp copy (files are
        // pre-downloaded for drag-out when a drag starts) is dead weight.
        if !dragged.is_dir {
            if let Some(tab) = self.active_tab() {
                let key = (tab.session_id, dragged.path.clone());
                if let Some(local) = self.temp_download_cache.lock().remove(&key) {
                    if let Some(dir) = local.parent() {
                        let _ = std::fs::remove_dir_all(dir);
                    }
                }
            }
        }
        let connected = self
            .active_tab()
            .is_some_and(|tab| tab.state == ConnState::Connected);
        if !connected {
            self.status = "not connected".into();
            cx.notify();
            return;
        }
        let Some(tab) = self.active_tab() else {
            return;
        };
        let Some(root) = tab.root_path.clone() else {
            return;
        };
        let Some(session) = tab.session.clone() else {
            return;
        };
        let session_id = tab.session_id;
        let target_dir = match &target {
            TreeDragTarget::Background => root,
            TreeDragTarget::Row { path, is_dir } => {
                if *is_dir {
                    path.clone()
                } else {
                    path.parent()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| root.clone())
                }
            }
        };
        if dragged.is_dir && target_dir.starts_with(dragged.path.as_path()) {
            self.status = "cannot move a directory into itself".into();
            cx.notify();
            return;
        }
        if dragged.path.parent() == Some(target_dir.as_path()) {
            return; // already in that directory
        }
        let to = target_dir.join(dragged.name.as_ref());
        self.active_tab_mut()
            .expect("checked above")
            .status = format!("moving {} → {}", dragged.path.display(), to.display());
        session.rename(session_id, dragged.path.clone(), to);
        cx.notify();
    }

    /// Zed auto-expands a collapsed directory that is hovered during a drag;
    /// expand after a short delay if the cursor is still over it.
    fn schedule_drag_expand(
        &mut self,
        path: PathBuf,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let collapsed_dir = self
            .active_tab_mut()
            .and_then(|tab| {
                let node = find_node(&mut tab.tree, &path)?;
                (node.entry.is_dir && !node.expanded).then_some(())
            })
            .is_some();
        if !collapsed_dir {
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            this.update_in(cx, move |this, window, cx| {
                let still_hovering = matches!(
                    &this.tree_drag_target,
                    Some(TreeDragTarget::Row { path: p, .. }) if *p == path
                ) && bounds.contains(&window.mouse_position());
                if !still_hovering {
                    return;
                }
                if let Some(tab) = this.active_tab_mut() {
                    if let Some(node) = find_node(&mut tab.tree, &path) {
                        if node.entry.is_dir && !node.expanded {
                            node.expanded = true;
                            if node.children.is_none() {
                                node.loading = true;
                                if let Some(session) = tab.session.as_ref() {
                                    session.list_dir(tab.session_id, path.clone());
                                }
                            }
                            cx.notify();
                        }
                    }
                }
            })
            .ok();
        })
        .detach();
    }
}

impl ProfileForm {
    fn blank(cx: &mut Context<RootView>) -> Self {
        Self::from_fields(cx, None, AuthKind::Password, "", "", "22", "", "", "", "")
    }

    fn from_profile(index: Option<usize>, profile: &Profile, cx: &mut Context<RootView>) -> Self {
        let (auth_kind, password, key_path, passphrase) = match &profile.auth {
            AuthMethod::Password { password } => {
                (AuthKind::Password, password.clone(), String::new(), String::new())
            }
            AuthMethod::KeyFile { path, passphrase } => (
                AuthKind::KeyFile,
                String::new(),
                path.display().to_string(),
                passphrase.clone().unwrap_or_default(),
            ),
            AuthMethod::Agent => (AuthKind::Agent, String::new(), String::new(), String::new()),
        };
        Self::from_fields(
            cx,
            index,
            auth_kind,
            &profile.name,
            &profile.host,
            &profile.port.to_string(),
            &profile.username,
            &password,
            &key_path,
            &passphrase,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_fields(
        cx: &mut Context<RootView>,
        editing: Option<usize>,
        auth_kind: AuthKind,
        name: &str,
        host: &str,
        port: &str,
        username: &str,
        password: &str,
        key_path: &str,
        passphrase: &str,
    ) -> Self {
        let mut field = |placeholder: &str, value: &str, masked: bool| {
            let field = cx.new(|cx| TextField::new(cx, placeholder.to_string()).masked(masked));
            field.update(cx, |field, cx| field.set_text(value.to_string(), cx));
            field
        };
        Self {
            editing,
            auth_kind,
            name: field("profile name", name, false),
            host: field("host or IP", host, false),
            port: field("22", port, false),
            username: field("username", username, false),
            password: field("password", password, true),
            key_path: field("~/.ssh/id_ed25519", key_path, false),
            passphrase: field("key passphrase (optional)", passphrase, true),
        }
    }
}

// --- terminal colors --------------------------------------------------------

// The terminal palette lives in the active theme: foreground/background and
// the 16 ANSI colors come from the theme JSON (`terminal.*` keys); see
// `theme.rs`.

fn rgb_to_hsla(color: Rgb) -> Hsla {
    rgb((color.r as u32) << 16 | (color.g as u32) << 8 | color.b as u32).into()
}

/// 256-color lookup: 0-15 palette, 16-231 cube, 232-255 grayscale.
fn indexed_color(index: u8) -> Hsla {
    match index {
        0..=7 => theme::ansi_normal()[index as usize],
        8..=15 => theme::ansi_bright()[index as usize - 8],
        16..=231 => {
            let idx = index - 16;
            let r = idx / 36;
            let g = (idx % 36) / 6;
            let b = idx % 6;
            let channel = |v: u8| if v == 0 { 0 } else { 55 + 40 * v as u32 };
            rgb(channel(r) << 16 | channel(g) << 8 | channel(b)).into()
        }
        232..=255 => {
            let level = 8 + 10 * (index - 232) as u32;
            rgb(level << 16 | level << 8 | level).into()
        }
    }
}

fn named_color(color: NamedColor, dim: bool) -> Hsla {
    let normal = theme::ansi_normal();
    let bright = theme::ansi_bright();
    let dims = theme::ansi_dim();
    match color {
        NamedColor::Foreground => {
            if dim {
                theme::term_fg_dim()
            } else {
                theme::term_fg()
            }
        }
        NamedColor::Background => theme::term_bg(),
        NamedColor::BrightForeground => theme::term_fg_bright(),
        NamedColor::Black => normal[0],
        NamedColor::Red => normal[1],
        NamedColor::Green => normal[2],
        NamedColor::Yellow => normal[3],
        NamedColor::Blue => normal[4],
        NamedColor::Magenta => normal[5],
        NamedColor::Cyan => normal[6],
        NamedColor::White => normal[7],
        NamedColor::BrightBlack => bright[0],
        NamedColor::BrightRed => bright[1],
        NamedColor::BrightGreen => bright[2],
        NamedColor::BrightYellow => bright[3],
        NamedColor::BrightBlue => bright[4],
        NamedColor::BrightMagenta => bright[5],
        NamedColor::BrightCyan => bright[6],
        NamedColor::BrightWhite => bright[7],
        NamedColor::DimBlack => dims[0],
        NamedColor::DimRed => dims[1],
        NamedColor::DimGreen => dims[2],
        NamedColor::DimYellow => dims[3],
        NamedColor::DimBlue => dims[4],
        NamedColor::DimMagenta => dims[5],
        NamedColor::DimCyan => dims[6],
        NamedColor::DimWhite => dims[7],
        // Cursor color and dim/bright foreground/background variants fall
        // back to the defaults.
        _ => theme::term_fg(),
    }
}

/// Resolve a cell's foreground/background to concrete colors.
fn cell_colors(cell: &Cell) -> (Hsla, Hsla) {
    let dim = cell.flags.contains(Flags::DIM);
    let mut fg = match cell.fg {
        Color::Spec(rgb) => rgb_to_hsla(rgb),
        Color::Indexed(index) => indexed_color(index),
        Color::Named(named) => named_color(named, dim),
    };
    let mut bg = match cell.bg {
        Color::Spec(rgb) => rgb_to_hsla(rgb),
        Color::Indexed(index) => indexed_color(index),
        Color::Named(named) => named_color(named, false),
    };
    if cell.flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    if cell.flags.contains(Flags::HIDDEN) {
        fg = bg;
    }
    (fg, bg)
}

/// True when the cell carries no colors of its own — the program never set
/// a foreground/background (nor inverse video), so heuristic syntax coloring
/// may paint it. Anything SGR-colored (`ls --color`, vim, htop, …) fails
/// this check and renders exactly as the program intended.
// [impl->req~uncolored-cell-coloring~1]
// [impl->feat~shell-syntax-coloring~1]
fn cell_is_uncolored(cell: &Cell) -> bool {
    matches!(cell.fg, Color::Named(NamedColor::Foreground))
        && matches!(cell.bg, Color::Named(NamedColor::Background))
        && !cell.flags.contains(Flags::INVERSE)
}

// --- terminal rendering -----------------------------------------------------

/// Style-relevant cell flags; runs merge only when all of these match.
const STYLE_FLAGS: Flags = Flags::BOLD
    .union(Flags::ITALIC)
    .union(Flags::DIM)
    .union(Flags::HIDDEN)
    .union(Flags::INVERSE)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::STRIKEOUT);

/// A horizontal run of same-styled cells on one terminal row.
struct RowRun {
    start_col: usize,
    /// Number of grid columns this run spans (wide chars count for two).
    span_cols: usize,
    text: String,
    fg: Hsla,
    bg: Hsla,
    bold: bool,
    italic: bool,
    underline: Option<UnderlineStyle>,
    strikethrough: bool,
}

/// What the terminal canvas prepaint computes and paint consumes.
struct TerminalPrepaint {
    lines: Vec<(Point<Pixels>, ShapedLine)>,
    backgrounds: Vec<gpui::PaintQuad>,
    cursor: Option<TerminalCursor>,
}

/// Zed-style terminal cursor: a quad over the cursor cell (filled, or an
/// outline when hollow) and, for a focused block cursor, the glyph under the
/// cursor repainted in the terminal background color on top of the quad.
struct TerminalCursor {
    origin: Point<Pixels>,
    quad: gpui::PaintQuad,
    block_text: Option<ShapedLine>,
}

/// A run of selected cells on one terminal row: (row, start_col, span).
type SelectionSegment = (usize, usize, usize);

/// Collect styled runs for every visible row of the terminal.
fn collect_runs(
    terminal: &TerminalModel,
    highlighter: Option<&LogHighlighter>,
    overlay: Option<&LogOverlay>,
) -> (
    Vec<Vec<RowRun>>,
    Option<(usize, usize, CursorShape, char)>,
    Vec<SelectionSegment>,
) {
    let term = terminal.term.lock();
    let content = term.renderable_content();
    let screen_lines = term.screen_lines();
    // alacritty's display points are negative for scrollback rows; shift
    // them into row-from-top space so scrolled-up views render correctly
    // (and selections map straight onto painted rows).
    let display_offset = content.display_offset as i32;
    let selection_range = term
        .selection
        .as_ref()
        .and_then(|selection| selection.to_range(&*term));
    let cursor = if content.mode.contains(TermMode::SHOW_CURSOR) && content.display_offset == 0 {
        Some((
            content.cursor.point.line.0.max(0) as usize,
            content.cursor.point.column.0,
            content.cursor.shape,
        ))
    } else {
        None
    };
    // Character under the cursor (for the block cursor's glyph); default to
    // whitespace so an empty cell still gets a full-width block.
    let mut cursor_char = ' ';

    // For log highlighting: rebuild each visual row's text, then precompute
    // per-row segments as char ranges with their colors.
    let mut row_highlights: Vec<Vec<(usize, usize, Hsla, bool)>> =
        (0..screen_lines).map(|_| Vec::new()).collect();
    if highlighter.is_some() {
        let mut row_texts: Vec<String> = (0..screen_lines).map(|_| String::new()).collect();
        let text_content = term.renderable_content();
        for indexed in text_content.display_iter {
            let line = indexed.point.line.0 + display_offset;
            if line < 0 || line as usize >= screen_lines {
                continue;
            }
            let is_spacer = indexed
                .cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER);
            if !is_spacer {
                row_texts[line as usize].push(indexed.cell.c);
            }
        }
        let highlighter = highlighter.expect("checked above");
        for (row, text) in row_texts.iter().enumerate() {
            for (range, fg, bold) in highlighter.highlight_line(text) {
                let start = text[..range.start].chars().count();
                let end = text[..range.end].chars().count();
                row_highlights[row].push((start, end, fg, bold));
            }
        }
    }

    let mut rows: Vec<Vec<RowRun>> = (0..screen_lines).map(|_| Vec::new()).collect();
    // Char index within each row, for mapping highlight segments onto cells.
    let mut row_char_ix: Vec<usize> = vec![0; screen_lines];
    let mut selection: Vec<SelectionSegment> = Vec::new();
    for indexed in content.display_iter {
        let cell: &Cell = indexed.cell;
        let line = indexed.point.line.0 + display_offset;
        if line < 0 || line as usize >= screen_lines {
            continue;
        }
        let flags = cell.flags & STYLE_FLAGS;
        // Spacer cells hold no text; the wide glyph of the previous cell
        // already covers their column.
        let is_spacer = cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER);
        let row = line as usize;
        let col = indexed.point.column.0;
        // Extend/merge the selection overlay for this cell. The hidden
        // cursor shape skips alacritty's "don't select at a block cursor"
        // boundary rule, which only matters for inverse-video rendering.
        if let Some(range) = selection_range {
            if range.contains_cell(&indexed, indexed.point, CursorShape::Hidden) {
                // Extend the run of overlay segments, but keep processing the
                // cell: its glyph must still be painted below the overlay.
                // Bailing out here used to swallow every selected cell's text
                // after the first one of each segment.
                let mut merged = false;
                if let Some((seg_row, seg_start, seg_span)) = selection.last_mut() {
                    if *seg_row == row && *seg_start + *seg_span == col {
                        *seg_span += 1;
                        merged = true;
                    }
                }
                if !merged {
                    selection.push((row, col, 1));
                }
            }
        }
        // Capture the glyph under the cursor, including the leading cell of a
        // wide char whose spacer occupies the cursor column.
        if let Some((cursor_line, cursor_col, _)) = cursor {
            if row == cursor_line && !is_spacer && (col == cursor_col || col + 1 == cursor_col) {
                cursor_char = cell.c;
            }
        }
        let char_ix = row_char_ix[row];
        if !is_spacer {
            row_char_ix[row] += 1;
        }
        let (mut fg, mut bg) = cell_colors(cell);
        let mut bold = flags.contains(Flags::BOLD);
        if cell_is_uncolored(cell) {
            if let Some(segments) = row_highlights.get(row) {
                if let Some((_, _, highlight_fg, highlight_bold)) = segments
                    .iter()
                    .find(|(start, end, _, _)| char_ix >= *start && char_ix < *end)
                {
                    fg = *highlight_fg;
                    bold |= *highlight_bold;
                }
            }
        }
        // Search matches paint a translucent accent background; the runs
        // merge check below splits at the boundary automatically.
        if let Some(overlay) = overlay {
            let grid_line = indexed.point.line.0;
            if let Some(ranges) = overlay.matches.get(&grid_line) {
                let matched = ranges
                    .iter()
                    .any(|(start, end)| char_ix >= *start && char_ix < *end);
                if matched {
                    bg = Hsla {
                        a: 0.30,
                        ..theme::accent()
                    };
                }
            }
            if let Some((cur_line, (cur_start, cur_end))) = overlay.current {
                if grid_line == cur_line && char_ix >= cur_start && char_ix < cur_end {
                    bg = Hsla {
                        a: 0.55,
                        ..theme::accent()
                    };
                }
            }
        }
        let underline = if flags.contains(Flags::UNDERCURL) {
            Some(UnderlineStyle {
                color: Some(fg),
                thickness: px(1.),
                wavy: true,
            })
        } else if flags.intersects(Flags::ALL_UNDERLINES) {
            Some(UnderlineStyle {
                color: Some(fg),
                thickness: px(1.),
                wavy: false,
            })
        } else {
            None
        };
        let italic = flags.contains(Flags::ITALIC);
        let strikethrough = flags.contains(Flags::STRIKEOUT);

        let runs = &mut rows[row];
        let mergeable = runs.last().is_some_and(|last| {
            last.bold == bold
                && last.italic == italic
                && last.strikethrough == strikethrough
                && last.underline == underline
                && colors_equal(last.fg, fg)
                && colors_equal(last.bg, bg)
                && last.start_col + last.span_cols == col
        });

        if mergeable {
            let last = runs.last_mut().unwrap();
            last.span_cols += 1;
            if !is_spacer {
                last.text.push(cell.c);
            }
        } else {
            runs.push(RowRun {
                start_col: col,
                span_cols: 1,
                text: if is_spacer {
                    String::new()
                } else {
                    cell.c.to_string()
                },
                fg,
                bg,
                bold,
                italic,
                underline,
                strikethrough,
            });
        }
    }
    (
        rows,
        cursor.map(|(line, col, shape)| (line, col, shape, cursor_char)),
        selection,
    )
}

/// Log-render bundle: overlays for grid mode, or the filter row set.
struct LogRender<'a> {
    overlay: Option<&'a LogOverlay>,
    /// Filter mode: anchored rows (oldest first) to display instead of the
    /// grid. `filter_offset` rows are scrolled up from the live edge.
    filter: Option<&'a [i64]>,
    filter_offset: usize,
    fed: i64,
}

/// Build styled runs for filter mode: only the anchored rows render, laid
/// out bottom-anchored like a terminal (so the live edge stays put while new
/// matching lines arrive). Each row is a sequence of runs with the log
/// highlighter's colors and search-match backgrounds applied.
fn collect_filtered_runs(
    terminal: &TerminalModel,
    highlighter: Option<&LogHighlighter>,
    render: &LogRender,
    screen_lines: usize,
) -> Vec<Vec<RowRun>> {
    let mut rows: Vec<Vec<RowRun>> = (0..screen_lines).map(|_| Vec::new()).collect();
    let Some(anchors) = render.filter else {
        return rows;
    };
    // Visible window: the last screen_lines anchors, shifted up by the
    // filter scroll offset, anchored to the bottom of the canvas.
    let end = anchors.len().saturating_sub(render.filter_offset);
    let start = end.saturating_sub(screen_lines);
    let visible = &anchors[start..end];
    let first_row = screen_lines - visible.len();
    let match_bg = Hsla {
        a: 0.30,
        ..theme::accent()
    };
    for (ix, anchor) in visible.iter().enumerate() {
        let line0 = (*anchor - render.fed) as i32;
        let Some((text, cols)) = terminal.row_text(line0) else {
            continue;
        };
        let high_segments: Vec<(usize, usize, Hsla, bool)> = highlighter
            .map(|highlighter| highlighter.highlight_line(&text))
            .unwrap_or_default()
            .into_iter()
            .map(|(range, fg, bold)| {
                (
                    text[..range.start].chars().count(),
                    text[..range.end].chars().count(),
                    fg,
                    bold,
                )
            })
            .collect();
        let match_segments: Vec<(usize, usize)> = render
            .overlay
            .and_then(|overlay| overlay.matches.get(&line0))
            .cloned()
            .unwrap_or_default();
        let mut runs: Vec<RowRun> = Vec::new();
        for (char_ix, ch) in text.chars().enumerate() {
            let col = cols[char_ix];
            let span = cols.get(char_ix + 1).copied().unwrap_or(col + 1) - col;
            let mut fg = theme::term_fg();
            let mut bg = theme::term_bg();
            let mut bold = false;
            for (start, end, seg_fg, seg_bold) in &high_segments {
                if char_ix >= *start && char_ix < *end {
                    fg = *seg_fg;
                    bold = *seg_bold;
                    break;
                }
            }
            if match_segments
                .iter()
                .any(|(start, end)| char_ix >= *start && char_ix < *end)
            {
                bg = match_bg;
            }
            filtered_push_run(&mut runs, col, span, ch, fg, bg, bold);
        }
        rows[first_row + ix] = runs;
    }
    rows
}

/// Push one cell's worth of text into a filter-mode run list, merging with
/// the previous run when style and geometry allow.
fn filtered_push_run(
    runs: &mut Vec<RowRun>,
    col: usize,
    span: usize,
    ch: char,
    fg: Hsla,
    bg: Hsla,
    bold: bool,
) {
    let mergeable = runs.last().is_some_and(|last| {
        last.bold == bold
            && colors_equal(last.fg, fg)
            && colors_equal(last.bg, bg)
            && last.underline.is_none()
            && last.start_col + last.span_cols == col
    });
    if mergeable {
        let last = runs.last_mut().unwrap();
        last.span_cols += span;
        last.text.push(ch);
    } else {
        runs.push(RowRun {
            start_col: col,
            span_cols: span,
            text: ch.to_string(),
            fg,
            bg,
            bold,
            italic: false,
            underline: None,
            strikethrough: false,
        });
    }
}

fn colors_equal(a: Hsla, b: Hsla) -> bool {
    a == b
}

impl RootView {
// [impl->feat~terminal-emulation~1]
    fn render_terminal(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(tab) = self.tabs.get(self.active) else {
            return div()
                .flex_1()
                .min_w(px(0.))
                .h_full()
                .bg(theme::term_bg())
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_color(theme::text_dim())
                        .child("select a profile in the sidebar and press Connect"),
                );
        };
        let terminal = tab.terminal.clone();
        let geometry = tab.geometry.clone();
        let focus_handle = tab.focus_handle.clone();
        let canvas_focus = focus_handle.clone();
        // Log tabs use their own rule set; shell tabs get the heuristic shell
        // coloring while the toggle is on (uncolored cells only).
        let highlighter = if tab.is_log() {
            tab.highlighter.clone()
        } else if self.shell_coloring {
            Some(self.shell_highlighter.clone())
        } else {
            None
        };
        let terminal_font_size = self.terminal_font_size;
        // Log tabs: per-frame overlay + optional filter row set. Built here
        // (UI thread, cheap) and shared with the canvas prepaint.
        let log_render = tab.log_view.as_ref().map(|view| {
            let fed = tab.terminal.lines_fed() as i64;
            let overlay = LogOverlay::build(view, fed);
            let filter = if view.filtering() {
                Some(Arc::new(view.filtered.clone()))
            } else {
                None
            };
            (Arc::new(overlay), filter, view.filter_offset, fed)
        });

        div()
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .bg(theme::term_bg())
            .track_focus(&focus_handle)
            .on_key_down(cx.listener(Self::on_terminal_key_down))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _window, cx| {
                // cmd+wheel zooms the terminal font on macOS, ctrl+wheel on
                // Windows/Linux (like Zed); a plain wheel scrolls the scrollback.
                #[cfg(target_os = "macos")]
                let zoom_wheel = event.modifiers.platform;
                #[cfg(not(target_os = "macos"))]
                let zoom_wheel = event.modifiers.platform || event.modifiers.control;
                if zoom_wheel {
                    let pixel_delta = event.delta.pixel_delta(px(20.));
                    let steps = (f32::from(pixel_delta.y) / 20.).round();
                    if steps != 0.0 {
                        this.zoom_terminal(steps, cx);
                    }
                    return;
                }
                // Positive y is wheel-up; alacritty scrolls into the
                // scrollback history for positive deltas, so pass it through.
                // `Scroll::Delta` is whole lines (i32); approximate the pixel
                // delta against the cell height.
                let line_height = this.terminal_font_size * TERMINAL_LINE_HEIGHT_RATIO;
                let pixel_delta = event.delta.pixel_delta(px(line_height));
                let lines = (f32::from(pixel_delta.y) / line_height).round() as i32;
                if lines != 0 {
                    if let Some(tab) = this.active_tab_mut() {
                        if let Some(view) = tab.log_view.as_mut() {
                            if view.filtering() {
                                // Filter mode scrolls its own row window;
                                // bottom-anchored, so a bigger offset shows
                                // older matching rows.
                                let len = view.filtered.len() as i32;
                                let max = len - 1;
                                let next =
                                    (view.filter_offset as i32 + lines).clamp(0, max.max(0));
                                view.filter_offset = next as usize;
                                view.follow = view.filter_offset == 0;
                                tab.terminal.mark_dirty();
                                cx.notify();
                                return;
                            }
                        }
                        tab.terminal.term.lock().scroll_display(Scroll::Delta(lines));
                        // Scrolling to the very bottom resumes following;
                        // anything else pauses at the current position.
                        if let Some(view) = tab.log_view.as_mut() {
                            let at_bottom = tab
                                .terminal
                                .term
                                .lock()
                                .renderable_content()
                                .display_offset
                                == 0;
                            view.follow = at_bottom;
                        }
                        tab.terminal.mark_dirty();
                        cx.notify();
                    }
                }
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, event: &MouseDownEvent, window, cx| {
                // A real press ends any OLE drag's stand-down period.
                this.ole_drag_active = false;
                if let Some(tab) = this.active_tab_mut() {
                    window.focus(&tab.focus_handle, cx);
                    let filtering = tab
                        .log_view
                        .as_ref()
                        .is_some_and(|view| view.filtering());
                    if let Some(view) = tab.log_view.as_mut() {
                        // Filter mode shows a synthetic row set; grid-based
                        // selection and caret don't apply there.
                        if !filtering {
                            if let Some(geometry) = tab.geometry.lock().as_ref().copied() {
                                let (line, _col, _side) =
                                    terminal_grid_point(&tab.terminal, geometry, event.position);
                                let fed = tab.terminal.lines_fed() as i64;
                                view.caret = Some(line as i64 + fed);
                            }
                        }
                    }
                    if !filtering {
                        // Begin a selection (a plain click clears the old one and
                        // selects nothing — `selection_end` drops empty ranges).
                        if let Some(geometry) = tab.geometry.lock().as_ref().copied() {
                            let (line, col, side) =
                                terminal_grid_point(&tab.terminal, geometry, event.position);
                            tab.terminal.selection_start(line, col, side);
                        }
                    }
                }
                cx.notify();
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| {
                if let Some(tab) = this.active_tab() {
                    tab.terminal.selection_end();
                }
                cx.notify();
            }))
            // Dropping OS files onto the terminal uploads them into the
            // active session's remote home directory.
            .can_drop(|value, _, _| value.is::<ExternalPaths>())
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                let paths = paths.paths().to_vec();
                let root = this.active_tab().and_then(|tab| tab.root_path.clone());
                match root {
                    Some(remote_dir) => this.upload_dropped_paths(&paths, remote_dir, cx),
                    None => {
                        this.status = "connect before dropping files to upload".into();
                        cx.notify();
                    }
                }
            }))
            .drag_over::<ExternalPaths>(|style, _, _, _| style.bg(theme::hover()))
            .child(
                canvas(
                    {
                        let log_render = log_render.clone();
                        move |bounds, window, _cx| {
                        let focused = canvas_focus.is_focused(window);
                        // Window-level mouse move (Zed's terminal does the
                        // same): with the button held and this terminal
                        // focused, extend the selection even when the cursor
                        // leaves the terminal bounds. The registration only
                        // lives for the next frame, so it is renewed on
                        // every prepaint.
                        window.on_mouse_event({
                            let terminal = terminal.clone();
                            let geometry = geometry.clone();
                            let canvas_focus = canvas_focus.clone();
                            move |event: &MouseMoveEvent, phase, window, _cx| {
                                if phase != DispatchPhase::Bubble
                                    || event.pressed_button != Some(MouseButton::Left)
                                    || !canvas_focus.is_focused(window)
                                    || !terminal.has_selection()
                                {
                                    return;
                                }
                                let Some(geo) = geometry.lock().as_ref().copied() else {
                                    return;
                                };
                                let (line, col, side) =
                                    terminal_grid_point(&terminal, geo, event.position);
                                terminal.selection_update(line, col, side);
                            }
                        });
                        let log = log_render.as_ref().map(|(overlay, filter, offset, fed)| {
                            LogRender {
                                overlay: Some(overlay.as_ref()),
                                filter: filter.as_ref().map(|rows| rows.as_slice()),
                                filter_offset: *offset,
                                fed: *fed,
                            }
                        });
                        terminal_prepaint(
                            bounds,
                            window,
                            &terminal,
                            &geometry,
                            highlighter.as_deref(),
                            log.as_ref(),
                            focused,
                            terminal_font_size,
                        )
                        }
                    },
                    move |_bounds, prepaint, window, cx| {
                        for quad in prepaint.backgrounds {
                            window.paint_quad(quad);
                        }
                        let line_height = prepaint_line_height(terminal_font_size);
                        for (origin, line) in prepaint.lines {
                            let _ = line.paint(
                                origin,
                                line_height,
                                gpui::TextAlign::Left,
                                None,
                                window,
                                cx,
                            );
                        }
                        if let Some(cursor) = prepaint.cursor {
                            window.paint_quad(cursor.quad);
                            if let Some(block_text) = cursor.block_text {
                                let _ = block_text.paint(
                                    cursor.origin,
                                    line_height,
                                    gpui::TextAlign::Left,
                                    None,
                                    window,
                                    cx,
                                );
                            }
                        }
                    },
                )
                .size_full(),
            )
    }
}

fn prepaint_line_height(font_size: f32) -> Pixels {
    px(font_size * TERMINAL_LINE_HEIGHT_RATIO)
}

/// Convert a window-space mouse position to an alacritty grid point plus
/// which half of the cell it sits in (that decides whether a selection
/// boundary includes the cell). Same mapping as Zed's terminal:
/// `line = row_from_top - display_offset`; negative lines are scrollback.
fn terminal_grid_point(
    terminal: &TerminalModel,
    geometry: TermGeometry,
    position: Point<Pixels>,
) -> (i32, usize, Direction) {
    let rel = position - geometry.bounds.origin;
    let x = f32::from(rel.x);
    let y = f32::from(rel.y);
    let cell_width = f32::from(geometry.cell_width);
    let line_height = f32::from(geometry.line_height);
    let term = terminal.term.lock();
    let display_offset = term.renderable_content().display_offset as i32;
    let rows = term.screen_lines() as i32;
    let last_col = term.columns() as i32 - 1;

    let mut col = (x / cell_width).floor() as i32;
    let mut row = (y / line_height).floor() as i32;
    let mut side = if col >= 0 && x / cell_width - col as f32 > 0.5 {
        Direction::Right
    } else {
        Direction::Left
    };
    if col > last_col {
        col = last_col.max(0);
        side = Direction::Right;
    } else if col < 0 {
        col = 0;
        side = Direction::Left;
    }
    if row > rows - 1 {
        row = (rows - 1).max(0);
        side = Direction::Right;
    } else if row < 0 {
        row = 0;
        side = Direction::Left;
    }
    (row - display_offset, col as usize, side)
}

fn terminal_prepaint(
    bounds: Bounds<Pixels>,
    window: &mut Window,
    terminal: &TerminalModel,
    geometry: &Arc<Mutex<Option<TermGeometry>>>,
    highlighter: Option<&LogHighlighter>,
    log: Option<&LogRender>,
    focused: bool,
    font_size: f32,
) -> TerminalPrepaint {
    // Note: `Window::scale_factor` is paint-phase-only in gpui, so the grid
    // origin cannot be device-pixel snapped here in prepaint. Text and
    // cursor share this same (possibly fractional) origin, which is what
    // keeps them aligned; Zed additionally snaps the origin, but only from
    // its layout/paint phases.
    let font = font(theme::FONT_MONO);
    let font_size = px(font_size);
    let line_height = prepaint_line_height(font_size.into());

    // Monospace cell width: the advance of 'm' (Zed's terminal measures
    // exactly this). A shaped probe line's width includes side bearings, so
    // dividing it by the char count mis-measures the grid the cursor and
    // `force_width` snapping rely on.
    let font_id = window.text_system().resolve_font(&font);
    let cell_width = window
        .text_system()
        .advance(font_id, font_size, 'm')
        .map(|advance| advance.width)
        .unwrap_or(px(8.));

    *geometry.lock() = Some(TermGeometry {
        bounds,
        cell_width,
        line_height,
    });

    // Filter mode replaces the data source; grid mode paints the terminal
    // with log overlays (search matches etc.).
    let (rows, cursor, selection, display_offset) = match log {
        Some(log) if log.filter.is_some() => {
            let screen = terminal.screen_lines();
            let rows = collect_filtered_runs(terminal, highlighter, log, screen);
            (rows, None, Vec::new(), 0i32)
        }
        _ => {
            let term = terminal.term.lock();
            let display_offset = term.renderable_content().display_offset as i32;
            drop(term);
            let (rows, cursor, selection) =
                collect_runs(terminal, highlighter, log.and_then(|l| l.overlay));
            (rows, cursor, selection, display_offset)
        }
    };
    // Same conversion path as `cell_colors`' default background, so plain
    // cells compare equal and skip their background fill.
    let default_bg = theme::term_bg();

    let mut lines = Vec::new();
    let mut backgrounds = Vec::new();

    // Log markers: a gutter bar on bookmarked rows, a full-width tint on the
    // current line (grid mode only — filter rows already stand out).
    if let Some(log) = log {
        if let Some(overlay) = log.overlay {
            let screen = terminal.screen_lines();
            let bookmark_color = theme::warning();
            for &line0 in &overlay.bookmarks {
                let row = line0 + display_offset;
                if row < 0 || row as usize >= screen {
                    continue;
                }
                let y = bounds.top() + line_height * row as f32;
                backgrounds.push(fill(
                    Bounds::new(point(bounds.left(), y), size(px(3.), line_height)),
                    bookmark_color,
                ));
            }
            if let Some(caret) = overlay.caret {
                let row = caret + display_offset;
                if row >= 0 && (row as usize) < screen {
                    let y = bounds.top() + line_height * row as f32;
                    backgrounds.push(fill(
                        Bounds::new(
                            point(bounds.left(), y),
                            size(bounds.size.width, line_height),
                        ),
                        Hsla {
                            a: 0.12,
                            ..theme::selection()
                        },
                    ));
                }
            }
        }
    }

    for (row_index, runs) in rows.iter().enumerate() {
        let y = bounds.top() + line_height * row_index as f32;
        for run in runs {
            let x = bounds.left() + cell_width * run.start_col as f32;
            if !colors_equal(run.bg, default_bg) {
                backgrounds.push(fill(
                    Bounds::new(
                        point(x, y),
                        size(cell_width * run.span_cols as f32, line_height),
                    ),
                    run.bg,
                ));
            }
            if run.text.is_empty() {
                continue;
            }
            let mut run_font = font.clone();
            if run.bold {
                run_font.weight = gpui::FontWeight::BOLD;
            }
            if run.italic {
                run_font.style = gpui::FontStyle::Italic;
            }
            let text: gpui::SharedString = run.text.clone().into();
            // No `force_width`: its base-glyph counter breaks on ligatures
            // (one glyph covering two cells), cramming the rest of the run.
            // With the cell width measured as the 'm' advance, monospace
            // shaping already lands on the cursor's grid.
            let shaped = window.text_system().shape_line(
                text.clone(),
                font_size,
                &[TextRun {
                    len: text.len(),
                    font: run_font,
                    color: run.fg,
                    background_color: None,
                    underline: run.underline,
                    strikethrough: run.strikethrough.then_some(gpui::StrikethroughStyle {
                        color: Some(run.fg),
                        thickness: px(1.),
                    }),
                }],
                None,
            );
            lines.push((point(x, y), shaped));
        }
    }

    // Selection overlay: translucent quads over the selected cells, above
    // cell backgrounds but below the text (Zed's terminal paints the same
    // way).
    let selection_color = Hsla {
        a: 0.5,
        ..theme::selection()
    };
    for (row, start_col, span) in selection {
        let x = bounds.left() + cell_width * start_col as f32;
        let y = bounds.top() + line_height * row as f32;
        backgrounds.push(fill(
            Bounds::new(point(x, y), size(cell_width * span as f32, line_height)),
            selection_color,
        ));
    }

    // Zed's terminal cursor (terminal_view/src/terminal_element.rs): the quad
    // covers the cursor cell; a focused block cursor additionally repaints
    // the glyph underneath in the terminal background color on top of it.
    let cursor = cursor.map(|(line, col, shape, cursor_char)| {
        let origin = point(
            bounds.left() + cell_width * col as f32,
            bounds.top() + line_height * line as f32,
        );
        let color = theme::cursor();

        let block_text = if focused && shape == CursorShape::Block {
            let text = cursor_char.to_string();
            let len = text.len();
            Some(window.text_system().shape_line(
                text.into(),
                font_size,
                &[TextRun {
                    len,
                    font: font.clone(),
                    color: theme::term_bg(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            ))
        } else {
            None
        };

        // Whitespace keeps the plain cell width; other characters cover at
        // least their cell (more for wide glyphs).
        let width = if cursor_char.is_whitespace() {
            cell_width
        } else {
            block_text
                .as_ref()
                .map(|text| text.width.max(cell_width))
                .unwrap_or(cell_width)
        };

        // Every shape becomes a hollow outline when the terminal is unfocused.
        let hollow = !focused || shape == CursorShape::HollowBlock;
        let cursor_bounds = if hollow {
            Bounds::new(origin, size(width, line_height))
        } else {
            match shape {
                CursorShape::Underline => Bounds::new(
                    point(origin.x, origin.y + line_height - px(2.)),
                    size(width, px(2.)),
                ),
                CursorShape::Beam => Bounds::new(origin, size(px(2.), line_height)),
                _ => Bounds::new(origin, size(width, line_height)),
            }
        };
        // No pixel-snapping here: the quad must share the text's exact
        // (fractional) origin, otherwise the marker drifts up to half a point
        // off the cell grid. Zed's terminal snaps only the element origin and
        // leaves cell offsets fractional for both text and cursor.
        let quad = if hollow {
            outline(cursor_bounds, color, BorderStyle::Solid)
        } else {
            fill(cursor_bounds, color)
        };
        TerminalCursor {
            origin,
            quad,
            block_text,
        }
    });

    TerminalPrepaint {
        lines,
        backgrounds,
        cursor,
    }
}

// --- layout -----------------------------------------------------------------

fn header_button(
    id: &'static str,
    label: &str,
    cx: &mut Context<RootView>,
    on_click: impl Fn(&mut RootView, &mut Window, &mut Context<RootView>) + 'static,
) -> gpui::AnyElement {
    div()
        .id(id)
        .child(label.to_string())
        .bg(theme::button())
        .text_color(theme::text())
        .hover(|style| style.bg(theme::button_hover()))
        .active(|style| style.opacity(0.8))
        .cursor_pointer()
        .flex()
        .px_3()
        .py_1()
        .rounded_sm()
        .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx)))
        .into_any_element()
}

/// Hover popup for icon-only controls: a small panel-styled text bubble.
struct TextTip(SharedString);

impl Render for TextTip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_sm()
            .bg(theme::panel())
            .border_1()
            .border_color(theme::border())
            .shadow_md()
            .text_xs()
            .text_color(theme::text())
            .child(self.0.clone())
    }
}

fn tip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView {
    let text = text.into();
    move |_, cx| cx.new(|_| TextTip(text.clone())).into()
}

/// One group of rows in the help overlay.
struct HelpSection {
    title: &'static str,
    rows: &'static [(&'static str, &'static str)],
}

/// The shortcut & feature reference shown by the header's help button.
/// Keep in sync with the actual bindings in `on_terminal_key_down` and the
/// file-tree/log-toolbar handlers.
// [impl->req~help-shortcut-list~1]
// [impl->feat~help-overlay~1]
const HELP_SECTIONS: &[HelpSection] = &[
    HelpSection {
        title: "Terminal",
        rows: &[
            ("Ctrl/⌘ + or -", "increase / decrease the terminal text size"),
            ("Ctrl/⌘ 0", "reset the terminal text size"),
            ("Ctrl/⌘ + scroll", "zoom the terminal text size"),
            ("scroll", "scroll the scrollback"),
            ("drag / double-click", "select text / select a word"),
            (
                "Ctrl/⌘ C with selection · Ctrl+Insert",
                "copy the selection (Ctrl+Shift+C works too)",
            ),
            ("Ctrl/⌘ V · Shift+Insert", "paste (Ctrl+Shift+V works too)"),
            ("Shift+↑ / Shift+↓", "recall cross-session command history"),
        ],
    },
    HelpSection {
        title: "File tree",
        rows: &[
            (
                "double-click file",
                "edit in a local editor; saving asks whether to sync back",
            ),
            (
                "right-click",
                "menu: tail -f, Download, Edit, VS Code, Rename, Delete — folders: Download as ZIP",
            ),
            ("drag & drop", "move remote entries; drop OS files to upload"),
            (".. row", "navigate to the parent directory"),
            ("Delete key", "delete the selected entry (asks first)"),
        ],
    },
    HelpSection {
        title: "Log follower tabs",
        rows: &[
            ("⌘/Ctrl F", "focus the search field"),
            ("Enter / Shift+Enter", "next / previous search match"),
            ("⌘/Ctrl G / Shift+G", "next / previous match (unfocused)"),
            ("⌘/Ctrl B", "bookmark the current line"),
            ("⌘/Ctrl [ / ]", "jump between bookmarks"),
            ("↑ ↓ PgUp PgDn Home End", "scroll the log"),
            ("select + copy keys", "log views are read-only but copyable"),
        ],
    },
    HelpSection {
        title: "General",
        rows: &[
            ("highlighter button", "toggle heuristic shell syntax coloring"),
            ("eye button", "toggle local echo (typing lag on slow links)"),
            ("theme dropdown", "switch between the bundled Zed themes"),
            (
                "REST API",
                "http://127.0.0.1:48920 — bearer token in the config dir (api_token); GET / lists endpoints",
            ),
        ],
    },
];

fn header_icon_button(
    id: &'static str,
    icon: &'static str,
    tooltip: &'static str,
    cx: &mut Context<RootView>,
    on_click: impl Fn(&mut RootView, &mut Window, &mut Context<RootView>) + 'static,
) -> gpui::AnyElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .w(px(28.))
        .h(px(26.))
        .rounded_sm()
        .bg(theme::button())
        .text_color(theme::text())
        .hover(|style| style.bg(theme::button_hover()))
        .active(|style| style.opacity(0.8))
        .cursor_pointer()
        .tooltip(tip(tooltip))
        .child(svg().path(icon).w(px(14.)).h(px(14.)).text_color(theme::text()))
        .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx)))
        .into_any_element()
}

fn section_label(label: &str) -> gpui::AnyElement {
    div()
        .px_2()
        .pt_3()
        .pb_1()
        .text_xs()
        .text_color(theme::text_dim())
        .child(label.to_string())
        .into_any_element()
}

fn form_row(label: &str, field: &Entity<TextField>) -> gpui::AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(
            div()
                .w(px(110.))
                .text_color(theme::text_dim())
                .child(label.to_string()),
        )
        .child(
            div()
                .flex_1()
                .border_1()
                .border_color(theme::border())
                .rounded_sm()
                .px_2()
                .py_1()
                .bg(theme::bg())
                .child(field.clone()),
        )
        .into_any_element()
}

impl RootView {
// [impl->feat~icon-ui~1]
    fn render_header(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let summary = self
            .selected
            .and_then(|i| self.store.profiles.get(i))
            .map(|p| format!("{} — {}", p.name, p.summary()))
            .unwrap_or_else(|| "no profile selected".to_string());

        div()
            .h(px(HEADER_HEIGHT))
            .w_full()
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border())
            .flex()
            .flex_row()
            .items_center()
            .px_3()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        svg()
                            .path(assets::ICON_MAIN_EXECUTABLE)
                            .w(px(20.))
                            .h(px(20.))
                            .text_color(theme::accent()),
                    )
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(theme::accent())
                            .child("aetherium"),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .text_color(theme::text_dim())
                    .truncate()
                    .child(summary),
            )
            .child(header_icon_button("new-profile", assets::ICON_PLUS, "New profile", cx, |this, window, cx| {
                this.open_new_profile_form(window, cx)
            }))
            .child(header_icon_button("edit-profile", assets::ICON_PENCIL, "Edit profile", cx, |this, window, cx| {
                this.open_edit_profile_form(window, cx)
            }))
            .child(header_icon_button("delete-profile", assets::ICON_TRASH, "Delete profile", cx, |this, _window, cx| {
                this.delete_selected_profile(cx)
            }))
            .child(header_icon_button(
                "toggle-echo",
                if self.local_echo {
                    assets::ICON_EYE
                } else {
                    assets::ICON_EYE_OFF
                },
                if self.local_echo {
                    "Local echo: on"
                } else {
                    "Local echo: off"
                },
                cx,
                |this, _window, cx| {
                    this.local_echo = !this.local_echo;
                    this.status = if this.local_echo {
                        "local echo on".into()
                    } else {
                        "local echo off".into()
                    };
                    cx.notify();
                },
            ))
            .child(
                div()
                    .id("toggle-shell-coloring")
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(28.))
                    .h(px(26.))
                    .rounded_sm()
                    .bg(theme::button())
                    .hover(|style| style.bg(theme::button_hover()))
                    .active(|style| style.opacity(0.8))
                    .cursor_pointer()
                    .tooltip(tip(if self.shell_coloring {
                        "Shell coloring: on"
                    } else {
                        "Shell coloring: off"
                    }))
                    .child(
                        svg()
                            .path(assets::ICON_HIGHLIGHTER)
                            .w(px(14.))
                            .h(px(14.))
                            .text_color(if self.shell_coloring {
                                theme::accent()
                            } else {
                                theme::text_dim()
                            }),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.shell_coloring = !this.shell_coloring;
                        this.save_ui_settings();
                        this.status = if this.shell_coloring {
                            "shell syntax coloring on (uncolored output only)".into()
                        } else {
                            "shell syntax coloring off".into()
                        };
                        for tab in &this.tabs {
                            tab.terminal.mark_dirty();
                        }
                        cx.notify();
                    })),
            )
            .child(header_icon_button(
                "help",
                assets::ICON_CIRCLE_HELP,
                "Shortcuts & features",
                cx,
                |this, _window, cx| {
                    this.help_open = !this.help_open;
                    cx.notify();
                },
            ))
            .child(
                div()
                    .id("theme")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .px_3()
                    .py_1()
                    .rounded_sm()
                    .bg(theme::button())
                    .text_color(theme::text())
                    .hover(|style| style.bg(theme::button_hover()))
                    .active(|style| style.opacity(0.8))
                    .cursor_pointer()
                    .tooltip(tip("Switch theme"))
                    .child(theme::active_name())
                    .child(
                        svg()
                            .path(assets::ICON_CHEVRON_DOWN)
                            .w(px(12.))
                            .h(px(12.))
                            .text_color(theme::text_dim()),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.theme_menu = !this.theme_menu;
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    /// Small icon-only button for the log toolbar.
fn log_toolbar_button(
    id: &'static str,
    icon: &'static str,
    tooltip_text: &'static str,
    cx: &mut Context<RootView>,
    on_click: impl Fn(&mut RootView, &mut Context<RootView>) + 'static,
) -> gpui::AnyElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .w(px(24.))
        .h(px(22.))
        .rounded_sm()
        .cursor_pointer()
        .tooltip(tip(tooltip_text))
        .child(svg().path(icon).w(px(13.)).h(px(13.)).text_color(theme::text()))
        .on_click(cx.listener(move |this, _, _window, cx| on_click(this, cx)))
        .into_any_element()
}

/// SnakeTail-style toolbar for the active log tab: follow/pause, search
    /// with match navigation, a filter, and bookmarks. Empty for other tabs.
// [impl->feat~snaketail-tools~1]
    fn render_log_toolbar(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return div().into_any_element();
        };
        if !tab.is_log() {
            return div().into_any_element();
        }
        let Some(view) = tab.log_view.as_mut() else {
            return div().into_any_element();
        };
        view.ensure_fields(cx);
        let search_field = view.search_field.clone().expect("created above");
        let filter_field = view.filter_field.clone().expect("created above");
        let (follow, match_count, current_match, filter_count, filtering, ended) = (
            view.follow,
            view.matches.len(),
            view.current_match,
            view.filtered.len(),
            view.filtering(),
            matches!(&tab.kind, TabKind::Log { ended: true, .. }),
        );

        let tool_input = |field: &Entity<TextField>| -> gpui::AnyElement {
            div()
                .w(px(180.))
                .px_2()
                .py(px(1.))
                .rounded_sm()
                .bg(theme::bg())
                .border_1()
                .border_color(theme::border())
                .child(field.clone())
                .into_any_element()
        };
        let divider = || {
            div()
                .w(px(1.))
                .h(px(16.))
                .mx_1()
                .bg(theme::border())
                .into_any_element()
        };

        let mut bar = div()
            .h(px(30.))
            .w_full()
            .px_2()
            .gap_1()
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border())
            .flex()
            .flex_row()
            .items_center()
            // Follow / pause.
            .child(Self::log_toolbar_button(
                "log-follow",
                if follow {
                    assets::ICON_PLAY_FILLED
                } else {
                    assets::ICON_DEBUG_PAUSE
                },
                if follow {
                    "Following — click to pause"
                } else {
                    "Paused — click to follow the live edge"
                },
                cx,
                |this, cx| this.log_toggle_follow(cx),
            ))
            .child(divider())
            // Search.
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(
                        svg()
                            .path(assets::ICON_SEARCH)
                            .w(px(13.))
                            .h(px(13.))
                            .text_color(theme::text_dim()),
                    )
                    .child(tool_input(&search_field)),
            )
            .child(
                div()
                    .min_w(px(48.))
                    .text_xs()
                    .text_color(theme::text_dim())
                    .child(if match_count > 0 {
                        format!(
                            "{}/{}",
                            current_match.map(|ix| ix + 1).unwrap_or(0),
                            match_count
                        )
                    } else {
                        String::new()
                    }),
            )
            .child(Self::log_toolbar_button(
                "log-prev-match",
                assets::ICON_ARROW_UP,
                "Previous match (⇧⌘G)",
                cx,
                |this, cx| this.log_search_navigate(false, cx),
            ))
            .child(Self::log_toolbar_button(
                "log-next-match",
                assets::ICON_ARROW_DOWN,
                "Next match (⌘G)",
                cx,
                |this, cx| this.log_search_navigate(true, cx),
            ))
            .child(divider())
            // Filter.
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(
                        svg()
                            .path(assets::ICON_FILTER)
                            .w(px(13.))
                            .h(px(13.))
                            .text_color(if filtering {
                                theme::accent()
                            } else {
                                theme::text_dim()
                            }),
                    )
                    .child(tool_input(&filter_field)),
            )
            .child(
                div()
                    .min_w(px(40.))
                    .text_xs()
                    .text_color(theme::text_dim())
                    .child(if filtering { format!("{filter_count}") } else { String::new() }),
            )
            .child(divider())
            // Bookmarks.
            .child(Self::log_toolbar_button(
                "log-bookmark",
                assets::ICON_BOOKMARK,
                "Toggle bookmark on the current line (⌘B)",
                cx,
                |this, cx| this.log_toggle_bookmark(cx),
            ))
            .child(Self::log_toolbar_button(
                "log-prev-bookmark",
                assets::ICON_ARROW_UP,
                "Previous bookmark (⌘[)",
                cx,
                |this, cx| this.log_bookmark_navigate(false, cx),
            ))
            .child(Self::log_toolbar_button(
                "log-next-bookmark",
                assets::ICON_ARROW_DOWN,
                "Next bookmark (⌘])",
                cx,
                |this, cx| this.log_bookmark_navigate(true, cx),
            ));
        if ended {
            bar = bar.child(
                div()
                    .ml_auto()
                    .px_2()
                    .text_xs()
                    .text_color(theme::text_dim())
                    .child("tail ended"),
            );
        }
        bar.into_any_element()
    }

    /// Row of tabs between header and content.
    fn render_tab_bar(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut bar = div()
            .h(px(30.))
            .w_full()
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border())
            .flex()
            .flex_row()
            .items_center()
            .px_1()
            .gap_1();
        if self.tabs.is_empty() {
            bar = bar.child(
                div()
                    .px_2()
                    .text_xs()
                    .text_color(theme::text_dim())
                    .child("no sessions — pick a profile and press Connect"),
            );
        }
        for (ix, tab) in self.tabs.iter().enumerate() {
            let (label, is_log) = match &tab.kind {
                TabKind::Shell => (
                    tab.profile
                        .as_ref()
                        .map(|profile| profile.name.clone())
                        .unwrap_or_else(|| "session".to_string()),
                    false,
                ),
                TabKind::Log { remote_path, ended, .. } => (
                    format!(
                        "⧉ {}{}",
                        remote_path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| remote_path.display().to_string()),
                        if *ended { " (ended)" } else { "" }
                    ),
                    true,
                ),
            };
            let is_active = ix == self.active;
            bar = bar.child(
                div()
                    .id(SharedString::from(format!("tab:{ix}")))
                    .px_2()
                    .py(px(1.))
                    .rounded_sm()
                    .cursor_pointer()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .max_w(px(220.))
                    .when(is_active, |tab| tab.bg(theme::selection()))
                    .hover(|tab| tab.bg(theme::hover()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.activate_tab(ix, cx);
                    }))
                    .child(
                        svg()
                            .path(if is_log {
                                assets::ICON_BACKGROUND_PROCESS
                            } else {
                                assets::ICON_CLI_TERMINAL
                            })
                            .w(px(14.))
                            .h(px(14.)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .truncate()
                            .when(is_log, |name| name.text_color(theme::text_dim()))
                            .child(label),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("tab-close:{ix}")))
                            .px_1()
                            .rounded_sm()
                            .text_color(theme::text_dim())
                            .hover(|style| style.text_color(theme::text()))
                            .child("✕")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.close_tab(ix, cx);
                            })),
                    ),
            );
        }
        bar.into_any_element()
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let content = match self.sidebar_tab {
            SidebarTab::Sessions => self.render_sessions(cx),
            SidebarTab::Files => self.render_active_files(cx),
            SidebarTab::Logs => self.render_logs(cx),
        };
        div()
            .w(self.sidebar_width)
            .h_full()
            .bg(theme::panel())
            .flex()
            .flex_col()
            .child(self.render_sidebar_tabs(cx))
            .child(content)
            .into_any_element()
    }

    /// Draggable splitter between the sidebar and the terminal.
    fn render_split_handle(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .id("sidebar-split")
            .w(px(4.))
            .h_full()
            .flex_none()
            .cursor(CursorStyle::ResizeLeftRight)
            .hover(|style| style.bg(theme::border()))
            .active(|style| style.bg(theme::accent()))
            .on_drag(SplitDrag, |_, _, _, cx| cx.new(|_| SplitDragView))
            .on_drag_move::<SplitDrag>(cx.listener(
                |this, event: &DragMoveEvent<SplitDrag>, _, cx| {
                    // The content row starts at the window's left edge, so the
                    // mouse position maps directly onto the sidebar width.
                    this.sidebar_width = (event.event.position.x - px(2.)).clamp(
                        px(180.),
                        px(640.),
                    );
                    cx.notify();
                },
            ))
            .into_any_element()
    }

    /// Tab switcher at the top of the sidebar: Sessions / Files / Logs.
    fn render_sidebar_tabs(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let active = self.sidebar_tab;
        let mut bar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .px_2()
            .pt_2()
            .border_b_1()
            .border_color(theme::border());
        for (tab, label) in [
            (SidebarTab::Sessions, "Sessions"),
            (SidebarTab::Files, "Files"),
            (SidebarTab::Logs, "Logs"),
        ] {
            let is_active = tab == active;
            bar = bar.child(
                div()
                    .id(SharedString::from(format!("sidebar-tab-{label}")))
                    .flex_1()
                    .py(px(5.))
                    .rounded_sm()
                    .cursor_pointer()
                    .text_xs()
                    .text_center()
                    .text_color(if is_active { theme::text() } else { theme::text_dim() })
                    .when(is_active, |item| item.bg(theme::selection()))
                    .when(!is_active, |item| item.hover(|style| style.bg(theme::hover())))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.sidebar_tab = tab;
                        if tab == SidebarTab::Logs {
                            this.logs_scroll_pending = true;
                        }
                        cx.notify();
                    })),
            );
        }
        bar.into_any_element()
    }

    /// The Sessions sidebar tab: saved profiles + recent connections.
    fn render_sessions(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .child(
                div()
                    .px_2()
                    .pt_3()
                    .pb_1()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(
                        svg()
                            .path(assets::ICON_CONFIGURATION)
                            .w(px(12.))
                            .h(px(12.))
                            .text_color(theme::text_dim()),
                    )
                    .child(div().text_xs().text_color(theme::text_dim()).child("PROFILES")),
            )
            .child(self.render_profile_list(cx))
            .child(
                div().px_2().pb_2().child(
                    div()
                        .id("connect")
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_center()
                        .gap_1()
                        .px_3()
                        .py_1()
                        .rounded_sm()
                        .bg(theme::button())
                        .text_color(theme::accent())
                        .hover(|style| style.bg(theme::button_hover()))
                        .active(|style| style.opacity(0.8))
                        .cursor_pointer()
                        .tooltip(tip("Connect to the selected profile"))
                        .child(
                            svg()
                                .path(assets::ICON_ARROW_RIGHT)
                                .w(px(13.))
                                .h(px(13.))
                                .text_color(theme::accent()),
                        )
                        .child("Connect")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.connect_selected(window, cx);
                        })),
                ),
            )
            .child(section_label("RECENT SESSIONS"))
            .child(self.render_recents(cx))
            .into_any_element()
    }

    /// The Logs sidebar tab: the tool's own log — connection issues,
    /// transfer/move errors, tail problems, and other failures.
    fn render_logs(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.logs_scroll_pending {
            self.log_scroll.scroll_to_bottom();
            self.logs_scroll_pending = false;
        }
        let mut section = div().flex().flex_col().flex_1().min_h(px(0.));
        section = section.child(
            div()
                .px_2()
                .pb_1()
                .pt_2()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_xs()
                        .text_color(theme::text_dim())
                        .child(format!("TOOL LOG — {} entries", self.logs.len())),
                )
                .child(header_button("log-clear", "clear", cx, |this, _window, cx| {
                    this.logs.clear();
                    cx.notify();
                })),
        );
        if self.logs.is_empty() {
            return section
                .child(
                    div()
                        .px_3()
                        .py_1()
                        .text_color(theme::text_dim())
                        .child("no log entries yet — connection and transfer issues show up here"),
                )
                .into_any_element();
        }
        let mut list = div()
            .id("log-list")
            .track_scroll(&self.log_scroll)
            .flex_1()
            .min_h(px(0.))
            .overflow_scroll()
            .flex()
            .flex_col()
            .gap(px(2.))
            .px_2()
            .pb_2();
        for entry in &self.logs {
            let (tag, color) = match entry.level {
                LogLevel::Info => ("INFO", theme::text_dim()),
                LogLevel::Warn => ("WARN", theme::warning()),
                LogLevel::Error => ("ERROR", theme::danger()),
            };
            list = list.child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap_2()
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(theme::text_dim())
                            .child(entry.time.clone()),
                    )
                    .child(
                        div()
                            .w(px(40.))
                            .flex_none()
                            .text_xs()
                            .text_color(color)
                            .child(tag),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_xs()
                            .text_color(theme::text())
                            .child(entry.message.clone()),
                    ),
            );
        }
        section.child(list).into_any_element()
    }

    /// The FILES sidebar section: session header (profile, download,
    /// disconnect) plus the active tab's file tree.
    fn render_active_files(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut section = div().flex().flex_col().flex_1().min_h(px(0.));
        let Some(tab) = self.active_tab() else {
            return section
                .child(
                    div()
                        .px_3()
                        .py_1()
                        .text_color(theme::text_dim())
                        .child("connect to browse files"),
                )
                .into_any_element();
        };
        match &tab.kind {
            TabKind::Log { remote_path, .. } => {
                section = section.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_color(theme::text_dim())
                        .child(format!("log view — {}", remote_path.display())),
                );
            }
            TabKind::Shell => {
                if tab.state != ConnState::Connected {
                    return section
                        .child(
                            div()
                                .px_3()
                                .py_1()
                                .text_color(theme::text_dim())
                                .child("connect to browse files"),
                        )
                        .into_any_element();
                }
                let summary = tab
                    .profile
                    .as_ref()
                    .map(|profile| profile.summary())
                    .unwrap_or_default();
                let show_details = self.show_file_details;
                section = section
                    .child(
                        div()
                            .px_2()
                            .pb_1()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .text_xs()
                                    .text_color(theme::text_dim())
                                    .truncate()
                                    .child(summary),
                            )
                            .child(header_button(
                                "tab-new-file",
                                "+f",
                                cx,
                                |this, window, cx| {
                                    this.start_create(false, window, cx);
                                },
                            ))
                            .child(header_button(
                                "tab-new-dir",
                                "+d",
                                cx,
                                |this, window, cx| {
                                    this.start_create(true, window, cx);
                                },
                            ))
                            .child(header_button(
                                "tab-download",
                                "⇩",
                                cx,
                                |this, _window, cx| {
                                    this.download_selected(cx);
                                },
                            ))
                            // Toggle the file-details column (sizes).
                            .child(
                                div()
                                    .id("tab-details")
                                    .child("≡")
                                    .bg(if show_details {
                                        theme::selection()
                                    } else {
                                        theme::button()
                                    })
                                    .text_color(if show_details {
                                        theme::text()
                                    } else {
                                        theme::text_dim()
                                    })
                                    .hover(|style| style.bg(theme::button_hover()))
                                    .active(|style| style.opacity(0.8))
                                    .cursor_pointer()
                                    .flex()
                                    .px_2()
                                    .py_1()
                                    .rounded_sm()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_file_details = !this.show_file_details;
                                        cx.notify();
                                    })),
                            )
                            .child(header_button(
                                "tab-disconnect",
                                "⏻",
                                cx,
                                |this, _window, cx| {
                                    this.disconnect(cx);
                                },
                            )),
                    )
                    .child(self.render_file_tree(cx));
            }
        }
        section.into_any_element()
    }

    /// Whether a connected shell tab exists for the given target.
    fn is_connected_for(&self, host: &str, port: u16, username: &str) -> bool {
        self.tabs.iter().any(|tab| {
            !tab.is_log()
                && tab.state == ConnState::Connected
                && tab
                    .profile
                    .as_ref()
                    .is_some_and(|p| p.host == host && p.port == port && p.username == username)
        })
    }

    fn render_recents(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let now = now_unix();
        let mut rows = Vec::new();
        for (ix, entry) in self.recents.entries.iter().enumerate() {
            let subtitle = format!("{} · {}", entry.profile_name, relative_time(now, entry.connected_at_unix));
            let connected = self.is_connected_for(&entry.host, entry.port, &entry.username);
            rows.push(
                div()
                    .id(SharedString::from(format!("recent:{ix}")))
                    .mx_2()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|row| row.bg(theme::hover()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.connect_recent(ix, window, cx);
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .child(
                                // Closed lock = a session is connected; open
                                // lock = not connected.
                                svg()
                                    .path(if connected {
                                        assets::ICON_LOCK
                                    } else {
                                        assets::ICON_LOCK_OFF
                                    })
                                    .w(px(13.))
                                    .h(px(13.))
                                    .text_color(if connected {
                                        theme::success()
                                    } else {
                                        theme::text_dim()
                                    }),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .truncate()
                                            .child(format!("{}@{}", entry.username, entry.host)),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme::text_dim())
                                            .truncate()
                                            .child(subtitle),
                                    ),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("recent-rm:{ix}")))
                                    .px_1()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .text_color(theme::text_dim())
                                    .hover(|style| style.text_color(theme::text()))
                                    .child("✕")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.remove_recent(ix, cx);
                                    })),
                            ),
                    )
                    .into_any_element(),
            );
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .px_3()
                    .py_1()
                    .text_color(theme::text_dim())
                    .child("no recent sessions")
                    .into_any_element(),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_1()
            .max_h(px(180.))
            .id("recent-list")
            .overflow_scroll()
            .children(rows)
            .into_any_element()
    }

    fn render_profile_list(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut rows = Vec::new();
        for (ix, profile) in self.store.profiles.iter().enumerate() {
            let is_selected = self.selected == Some(ix);
            let connected = self.is_connected_for(&profile.host, profile.port, &profile.username);
            rows.push(
                div()
                    .id(ix)
                    .mx_2()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .when(is_selected, |row| row.bg(theme::selection()))
                    .hover(|row| row.bg(theme::hover()))
                    // Single click selects; double click connects right away
                    // (MobaXterm habit).
                    .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                        this.selected = Some(ix);
                        let double = matches!(
                            event,
                            gpui::ClickEvent::Mouse(click) if click.down.click_count >= 2
                        );
                        if double {
                            if let Some(profile) = this.store.profiles.get(ix).cloned() {
                                this.connect_profile(profile, window, cx);
                                return;
                            }
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .child(
                                // Closed lock = a session is connected; open
                                // lock = not connected.
                                svg()
                                    .path(if connected {
                                        assets::ICON_LOCK
                                    } else {
                                        assets::ICON_LOCK_OFF
                                    })
                                    .w(px(13.))
                                    .h(px(13.))
                                    .text_color(if connected {
                                        theme::success()
                                    } else {
                                        theme::text_dim()
                                    }),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .min_w(px(0.))
                                    .child(profile.name.clone())
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme::text_dim())
                                            .child(profile.summary()),
                                    ),
                            ),
                    )
                    .into_any_element(),
            );
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .px_3()
                    .py_1()
                    .text_color(theme::text_dim())
                    .child("No profiles yet — click + New")
                    .into_any_element(),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_1()
            .max_h(px(220.))
            .id("profile-list")
            .overflow_scroll()
            .children(rows)
            .into_any_element()
    }

    /// Re-root the file tree at the current root's parent (".."
    /// navigation, MobaXterm style). The listing handler swaps the root
    /// when the parent's entries arrive.
    // [impl->req~tree-parent-navigation~1]
    fn navigate_tree_up(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        if tab.state != ConnState::Connected {
            return;
        }
        let Some(root) = tab.root_path.clone() else {
            return;
        };
        let Some(parent) = root.parent().map(PathBuf::from) else {
            return;
        };
        if parent == root {
            return; // filesystem root has no "up"
        }
        let Some(session) = tab.session.clone() else {
            return;
        };
        let session_id = tab.session_id;
        tab.pending_tree_root = Some(parent.clone());
        session.list_dir(session_id, parent);
        cx.notify();
    }

    /// Render an "up one level" row at the top of the file tree when the
    /// current root has a parent directory.
    fn tree_up_row(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let can_go_up = self.active_tab().is_some_and(|tab| {
            let Some(root) = tab.root_path.as_ref() else {
                return false;
            };
            root.parent().is_some_and(|parent| parent != root.as_path())
        });
        if !can_go_up {
            return None;
        }
        Some(
            div()
                .id("tree-up")
                .h(px(24.))
                .ml(px(4.))
                .mr(px(4.))
                .px(px(6.))
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .rounded_sm()
                .cursor_pointer()
                .tooltip(tip("Up one level"))
                .hover(|row| row.bg(theme::hover()))
                .child(
                    svg()
                        .path(assets::ICON_ARROW_UP)
                        .w(px(14.))
                        .h(px(14.))
                        .text_color(theme::text_dim()),
                )
                .child(
                    div()
                        .text_color(theme::text_dim())
                        .child(".."),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.navigate_tree_up(cx);
                }))
                .into_any_element(),
        )
    }

    fn render_file_tree(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut rows = Vec::new();
        // Built before the immutable borrows below (it needs &mut self).
        let up_row = self.tree_up_row(cx);
        let empty: Vec<TreeNode> = Vec::new();
        let (tree, tree_selection, connected, tree_focus) = match self.active_tab() {
            Some(tab) => (
                &tab.tree,
                tab.tree_selection.clone(),
                tab.state == ConnState::Connected,
                Some(tab.tree_focus_handle.clone()),
            ),
            None => (&empty, None, false, None),
        };
        // Directory highlighted as the current drop target during a drag.
        let drag_highlight = match (self.tree_dragging.as_ref(), self.tree_drag_target.as_ref()) {
            (Some(dragged), Some(target)) => self.tree_drop_highlight(dragged, target),
            _ => None,
        };
        if tree.is_empty() {
            rows.push(
                div()
                    .px_3()
                    .py_1()
                    .text_color(theme::text_dim())
                    .child(if connected { "loading…" } else { "connect to browse files" })
                    .into_any_element(),
            );
        } else {
            if let Some(up_row) = up_row {
                rows.push(up_row);
            }
            let tab = self.active_tab();
            render_tree_rows(
                tree,
                0,
                tree_selection.as_ref(),
                drag_highlight.as_ref(),
                self.show_file_details,
                tab.and_then(|t| t.session.as_ref()),
                tab.map(|t| t.session_id).unwrap_or(0),
                self.temp_download_cache.clone(),
                self.tree_editor.as_ref(),
                cx,
                &mut rows,
            );
        }
        div()
            .flex_1()
            .min_h(px(0.))
            .id("file-tree")
            .overflow_scroll()
            .flex()
            .flex_col()
            // Focus + key context for tree shortcuts (Delete/Backspace ask to
            // delete the selection, Enter/Escape answer the confirmation).
            // Only present for a real tab so the keys never dispatch here
            // when there is no tree.
            .when_some(tree_focus, |div, handle| {
                div.track_focus(&handle)
                    .key_context("FileTree")
                    .on_action(cx.listener(Self::on_delete_entry))
                    .on_action(cx.listener(Self::on_cancel_delete))
                    .on_action(cx.listener(Self::on_confirm_delete))
                    .on_action(cx.listener(Self::on_rename_entry))
            })
            // Dropping OS files onto the tree background uploads them into
            // the remote home directory; dropping onto a directory row
            // targets that directory instead (handled per row). Dropping a
            // dragged tree entry onto the background moves it into the root.
            .can_drop(|value, _, _| value.is::<ExternalPaths>() || value.is::<DraggedEntry>())
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                let paths = paths.paths().to_vec();
                let root = this.active_tab().and_then(|tab| tab.root_path.clone());
                match root {
                    Some(remote_dir) => this.upload_dropped_paths(&paths, remote_dir, cx),
                    None => {
                        this.status = "connect before dropping files to upload".into();
                        cx.notify();
                    }
                }
            }))
            .on_drop(cx.listener(|this, dragged: &DraggedEntry, _, cx| {
                if this.ole_drag_active {
                    return;
                }
                this.drop_tree_entry(dragged.clone(), TreeDragTarget::Background, cx);
            }))
            .on_drag_move::<DraggedEntry>(cx.listener(
                |this, event: &DragMoveEvent<DraggedEntry>, _, cx| {
                    if this.ole_drag_active {
                        return;
                    }
                    let is_current = matches!(this.tree_drag_target, Some(TreeDragTarget::Background));
                    if event.bounds.contains(&event.event.position) {
                        if !is_current {
                            this.tree_dragging = Some(event.drag(cx).clone());
                            this.tree_drag_target = Some(TreeDragTarget::Background);
                            cx.notify();
                        }
                    } else if is_current {
                        this.tree_drag_target = None;
                        cx.notify();
                    }
                },
            ))
            .drag_over::<ExternalPaths>(|style, _, _, _| style.bg(theme::drop_target()))
            .children(rows)
            .into_any_element()
    }

    fn render_form(form: &ProfileForm, cx: &mut Context<Self>) -> gpui::AnyElement {
        let title = if form.editing.is_some() {
            "Edit profile"
        } else {
            "New profile"
        };

        let auth_kind = form.auth_kind;
        let mut auth_buttons = Vec::new();
        for (kind, label) in [
            (AuthKind::Password, "Password"),
            (AuthKind::KeyFile, "Key file"),
            (AuthKind::Agent, "Agent"),
        ] {
            let active = kind == auth_kind;
            auth_buttons.push(
                div()
                    .id(label)
                    .px_3()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .bg(if active { theme::accent() } else { theme::button() })
                    .hover(|style| style.bg(theme::button_hover()))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(form) = this.form.as_mut() {
                            form.auth_kind = kind;
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }

        let mut rows = vec![
            form_row("Name", &form.name),
            form_row("Host", &form.host),
            form_row("Port", &form.port),
            form_row("Username", &form.username),
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .w(px(110.))
                        .text_color(theme::text_dim())
                        .child("Auth"),
                )
                .children(auth_buttons)
                .into_any_element(),
        ];
        match form.auth_kind {
            AuthKind::Password => rows.push(form_row("Password", &form.password)),
            AuthKind::KeyFile => {
                rows.push(form_row("Key file", &form.key_path));
                rows.push(form_row("Passphrase", &form.passphrase));
            }
            AuthKind::Agent => {}
        }

        div()
            .w_full()
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border())
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            // Tab / Shift+Tab move focus between the form's text fields; the
            // actions bubble up here from whichever field is focused.
            .on_action(cx.listener(|this, _: &Tab, window, cx| {
                this.form_tab(false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &Backtab, window, cx| {
                this.form_tab(true, window, cx);
            }))
            .child(
                div()
                    .font_weight(gpui::FontWeight::BOLD)
                    .child(title),
            )
            .children(rows)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .justify_end()
                    .child(header_button("cancel-form", "Cancel", cx, |this, _window, cx| {
                        this.cancel_form(cx)
                    }))
                    .child(header_button("save-form", "Save", cx, |this, _window, cx| {
                        this.save_form(cx)
                    })),
            )
            .into_any_element()
    }

// [impl->feat~transfer-progress~1]
    fn render_statusbar(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        // Show the active tab's lifecycle, transfer, and status; with no
        // tabs the global (UI-level) status stands alone.
        let (state, transfer, status) = match self.active_tab() {
            Some(tab) => (
                tab.display_state(),
                tab.transfer.clone(),
                if tab.status.is_empty() {
                    self.status.clone()
                } else {
                    tab.status.clone()
                },
            ),
            None => (ConnState::Disconnected, None, self.status.clone()),
        };
        let (state_icon, state_text, state_color) = match state {
            ConnState::Disconnected => (assets::ICON_DISCONNECTED, "disconnected", theme::text_dim()),
            ConnState::Connecting => (assets::ICON_SIGNAL_MEDIUM, "connecting…", theme::warning()),
            ConnState::Connected => (assets::ICON_SIGNAL_HIGH, "connected", theme::success()),
        };
        // Active transfer progress, shown between the state and the status
        // message: a graphical bar when the total is known, plus the
        // label/size/speed/ETA text and a cancel button.
        let transfer_bar = transfer
            .as_ref()
            .filter(|(_, _, total, _, _)| *total > 0)
            .map(|(_, done, total, _, _)| (*done as f64 / *total as f64).min(1.0));
        let transfer_text = transfer.as_ref().map(|(label, done, total, bps, eta)| {
            let mut parts = Vec::new();
            parts.push(label.clone());
            if *total > 0 {
                let pct = (*done as f64 / *total as f64 * 100.0).min(100.0) as u64;
                parts.push(format!("{}/{} ({}%)", format_size(*done), format_size(*total), pct));
            } else {
                parts.push(format_size(*done));
            }
            if *bps > 0.0 {
                parts.push(format!("{}/s", format_size(*bps as u64)));
            }
            if *eta > 0 {
                parts.push(format!("ETA {}", format_duration(*eta)));
            }
            parts.join(" — ")
        });
        // Dropped shell tab with a known profile: offer a one-click way
        // back (phone sessions disconnect constantly).
        let can_reconnect = self.active_tab().is_some_and(|tab| {
            !tab.is_log() && tab.profile.is_some() && tab.state == ConnState::Disconnected
        });
        div()
            .h(px(STATUSBAR_HEIGHT))
            .w_full()
            .bg(theme::panel())
            .border_t_1()
            .border_color(theme::border())
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px_3()
            .text_xs()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .text_color(state_color)
                    .child(
                        svg()
                            .path(state_icon)
                            .w(px(12.))
                            .h(px(12.))
                            .text_color(state_color),
                    )
                    .child(state_text),
            )
            .when_some(transfer_bar, |bar, pct| {
                const BAR_WIDTH: f32 = 120.;
                bar.child(
                    div()
                        .id("transfer-bar")
                        .flex_none()
                        .w(px(BAR_WIDTH))
                        .h(px(6.))
                        .rounded_sm()
                        .bg(theme::border())
                        .child(
                            div()
                                .h_full()
                                .w(px(BAR_WIDTH * pct as f32))
                                .rounded_sm()
                                .bg(theme::accent()),
                        ),
                )
            })
            .when_some(transfer_text, |bar, text| {
                bar.child(
                    div()
                        .flex_1()
                        .text_color(theme::warning())
                        .truncate()
                        .child(text),
                )
                .child(
                    div()
                        .id("transfer-cancel")
                        .flex_none()
                        .px_1()
                        .cursor_pointer()
                        .text_color(theme::text_dim())
                        .hover(|style| style.text_color(theme::danger()))
                        .child("✕")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cancel_transfer(cx);
                        })),
                )
            })
            .when(can_reconnect, |bar| {
                bar.child(
                    div()
                        .id("status-reconnect")
                        .px_2()
                        .py(px(1.))
                        .rounded_sm()
                        .cursor_pointer()
                        .border_1()
                        .border_color(theme::accent())
                        .text_color(theme::accent())
                        .hover(|style| style.bg(theme::hover()))
                        .child("Reconnect")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.reconnect_tab(cx);
                        })),
                )
            })
            .child(
                div()
                    .text_color(theme::text_dim())
                    .truncate()
                    .child(status),
            )
            .into_any_element()
    }
}

/// Human-friendly file size (e.g. `1.2K`).
fn format_size(size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024. && unit < UNITS.len() - 1 {
        value /= 1024.;
        unit += 1;
    }
    if unit == 0 {
        format!("{}B", size)
    } else {
        format!("{:.1}{}", value, UNITS[unit])
    }
}

/// Human-friendly duration in seconds (e.g. `2m 15s`).
fn format_duration(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

/// Recursively flatten the visible part of the tree into indented rows.
// [impl->feat~file-tree~1]
fn render_tree_rows(
    nodes: &[TreeNode],
    depth: usize,
    selected: Option<&PathBuf>,
    drag_highlight: Option<&PathBuf>,
    show_details: bool,
    session: Option<&SessionHandle>,
    session_id: u64,
    temp_download_cache: TempDownloadCache,
    editor: Option<&TreeEditor>,
    cx: &mut Context<RootView>,
    rows: &mut Vec<gpui::AnyElement>,
) {
    for node in nodes {
        let path = node.entry.path.clone();
        let is_dir = node.entry.is_dir;
        let is_selected = selected == Some(&node.entry.path);
        let highlighted = is_dir && drag_highlight == Some(&node.entry.path);
        // Loaded directories with no children disclose nothing: no chevron.
        // Unloaded ones keep theirs (contents unknown — may expand).
        let expandable = node
            .children
            .as_ref()
            .map(|children| !children.is_empty())
            .unwrap_or(true);
        let row_path = path.clone();
        let drag_move_path = path.clone();
        let external_drop_path = path.clone();
        let tree_drop_path = path.clone();
        let temp_download_cache = temp_download_cache.clone();
        let weak_root = cx.weak_entity();
        #[cfg(not(windows))]
        let _ = &weak_root;

        // Renaming this entry: swap its row for the inline editor.
        if let Some(ed) = editor.filter(|ed| ed.target.as_ref() == Some(&node.entry.path)) {
            rows.push(tree_editor_row(ed, depth, is_dir));
            continue;
        }

        rows.push(
            div()
                .id(SharedString::from(format!("tree:{}", node.entry.path.display())))
                .h(px(24.))
                .ml(px(4. + depth as f32 * 14.))
                .mr(px(4.))
                .px(px(6.))
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .rounded_sm()
                .cursor_pointer()
                .when(is_selected, |row| row.bg(theme::selection()))
                .when(!is_selected, |row| row.hover(|row| row.bg(theme::hover())))
                .when(highlighted, |row| row.bg(theme::drop_target()))
                .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    this.focus_file_tree(window, cx);
                    // Double-click a file to edit it locally (MobaXterm
                    // style: staged to a temp file, opened in the local
                    // editor, saves ask to sync back). Single click just
                    // selects/expands; tail -f lives in the right-click menu.
                    let double = matches!(
                        event,
                        gpui::ClickEvent::Mouse(click) if click.down.click_count >= 2
                    );
                    if !is_dir && double {
                        this.start_remote_edit(path.clone(), EditorChoice::Default, cx);
                    } else {
                        this.toggle_tree_node(path.clone(), cx);
                    }
                }))
                // Start dragging this entry; a small label follows the cursor
                // (Zed's project-panel drag image). Files also start a
                // background temp download so drag-out can offer a real local
                // path when the pointer leaves the window.
                .on_drag(
                    DraggedEntry {
                        path: row_path.clone(),
                        is_dir,
                        name: node.entry.name.clone().into(),
                    },
                    {
                        let remote_for_download = row_path.clone();
                        let session_for_download = session.cloned();
                        let session_id = session_id;
                        let cache_for_download = temp_download_cache.clone();
                        move |drag, click_offset, window, cx| {
                            #[cfg(not(windows))]
                            let _ = &window;
                            // Kick off a temp download in the background so
                            // the file is (hopefully) ready by the time the
                            // drag leaves the window. An already-staged copy
                            // is reused: re-staging would delete the path a
                            // previous drag may still be handing to the OS.
                            if !drag.is_dir {
                                if let Some(session) = session_for_download.as_ref() {
                                    let key = (session_id, remote_for_download.clone());
                                    if !cache_for_download.lock().contains_key(&key) {
                                        log::info!(
                                            "drag-out: staging {} for session {session_id}",
                                            remote_for_download.display()
                                        );
                                        session.download_to_temp(
                                            session_id,
                                            remote_for_download.clone(),
                                            cache_for_download.clone(),
                                        );
                                    }
                                    #[cfg(windows)]
                                    {
                                        // gpui has no outgoing file drags on
                                        // Windows — run our own OLE drag with
                                        // the staged path, and tell the
                                        // internal drop/hover handlers to
                                        // stand down while it owns the mouse.
                                        use raw_window_handle::{
                                            HasWindowHandle as _, RawWindowHandle,
                                        };
                                        let hwnd = match window
                                            .window_handle()
                                            .map(|handle| handle.as_raw())
                                        {
                                            Ok(RawWindowHandle::Win32(handle)) => {
                                                handle.hwnd.get() as isize
                                            }
                                            _ => 0,
                                        };
                                        let cache = cache_for_download.clone();
                                        let weak_root = weak_root.clone();
                                        crate::windows_drag::begin_file_drag(
                                            hwnd,
                                            std::sync::Arc::new(move || {
                                                cache.lock().get(&key).cloned()
                                            }),
                                        );
                                        let _ = weak_root.update(cx, |this, _cx| {
                                            this.ole_drag_active = true;
                                        });
                                    }
                                }
                            }
                            cx.new(|_| DraggedEntryView {
                                name: drag.name.clone(),
                                is_dir: drag.is_dir,
                                click_offset,
                            })
                        }
                    },
                )
// [impl->feat~os-file-drag-out~1]
                // When the drag leaves the window, offer the local temp path
                // to the OS as a native file drag. Only works if the
                // background download has already finished.
                .external_drag_payload({
                    let cache = temp_download_cache.clone();
                    let weak_root = cx.weak_entity();
                    move |drag: &DraggedEntry, _window, cx| {
                        if drag.is_dir {
                            let name = drag.name.to_string();
                            let _ = weak_root.update(cx, |this, cx| {
                                this.status =
                                    format!("drag-out: {name} is a folder — only files can be dragged out");
                                cx.notify();
                            });
                            return None;
                        }
                        let key = (session_id, drag.path.clone());
                        // The staging download publishes its result directly
                        // into this map from the backend thread. gpui resolves
                        // the payload exactly once, the first time the pointer
                        // leaves the window, so wait for it (bounded) rather
                        // than letting the drop die: blocking here freezes the
                        // drag image briefly, a dead drag is worse.
                        let deadline = std::time::Instant::now() + Duration::from_secs(5);
                        let local = loop {
                            if let Some(local) = cache.lock().get(&key) {
                                break Some(local.clone());
                            }
                            if std::time::Instant::now() >= deadline {
                                break None;
                            }
                            std::thread::sleep(Duration::from_millis(50));
                        };
                        let payload = local.map(|local| {
                            ExternalDragPayload::Files(FileDragPaths::new([(
                                local,
                                false,
                            )]))
                        });
                        log::info!(
                            "drag-out: resolve {} (session {session_id}) → {}",
                            drag.path.display(),
                            if payload.is_some() { "hit" } else { "miss" }
                        );
                        if payload.is_none() {
                            // The staging download is still running after the
                            // wait window — say so instead of failing silently.
                            // A retry drag will hit the cache once it lands.
                            let name = drag.name.to_string();
                            let _ = weak_root.update(cx, |this, cx| {
                                this.status =
                                    format!("still staging {name} — retry the drag in a moment");
                                cx.notify();
                            });
                        }
                        payload
                    }
                })
                .on_drag_move::<DraggedEntry>(cx.listener(
                    move |this, event: &DragMoveEvent<DraggedEntry>, window, cx| {
                        if this.ole_drag_active {
                            return;
                        }
                        let is_current = matches!(
                            &this.tree_drag_target,
                            Some(TreeDragTarget::Row { path, .. }) if path == &drag_move_path
                        );
                        if !event.bounds.contains(&event.event.position) {
                            // The row that set the target also clears it once
                            // the cursor leaves (Zed clears per entry).
                            if is_current {
                                this.tree_drag_target = None;
                                cx.notify();
                            }
                            return;
                        }
                        if is_current {
                            return;
                        }
                        this.tree_dragging = Some(event.drag(cx).clone());
                        this.tree_drag_target = Some(TreeDragTarget::Row {
                            path: drag_move_path.clone(),
                            is_dir,
                        });
                        cx.notify();
                        this.schedule_drag_expand(drag_move_path.clone(), event.bounds, window, cx);
                    },
                ))
                // Directory rows accept OS file drops and upload into that
                // directory (recursively); file rows accept them into the
                // file's parent directory.
                .can_drop(|value, _, _| value.is::<ExternalPaths>() || value.is::<DraggedEntry>())
                .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                    let target = if is_dir {
                        external_drop_path.clone()
                    } else {
                        external_drop_path
                            .parent()
                            .map(PathBuf::from)
                            .unwrap_or_else(|| external_drop_path.clone())
                    };
                    this.upload_dropped_paths(&paths.paths().to_vec(), target, cx);
                }))
                .on_drop(cx.listener(move |this, dragged: &DraggedEntry, _, cx| {
                    if this.ole_drag_active {
                        return;
                    }
                    this.drop_tree_entry(
                        dragged.clone(),
                        TreeDragTarget::Row {
                            path: tree_drop_path.clone(),
                            is_dir,
                        },
                        cx,
                    );
                }))
                .drag_over::<ExternalPaths>(|style, _, _, _| style.bg(theme::drop_target()))
                // Right-click for the context menu: files get the full
                // action set, directories the ZIP download.
                .on_mouse_down(
                    MouseButton::Right,
                    {
                        let menu_path = node.entry.path.clone();
                        let menu_is_dir = node.entry.is_dir;
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.focus_file_tree(window, cx);
                            if let Some(tab) = this.active_tab_mut() {
                                tab.tree_selection = Some(menu_path.clone());
                            }
                            this.context_menu =
                                Some((event.position, menu_path.clone(), menu_is_dir));
                            cx.notify();
                        })
                    },
                )
                .child(
                    // Leading glyph: disclosure chevron for directories,
                    // a file icon for files (Zed project-panel layout).
                    // Directories that loaded with no children get no
                    // chevron — nothing to disclose. Unloaded directories
                    // keep theirs: the contents are still unknown.
                    if is_dir && expandable {
                        svg()
                            .path(assets::ICON_CHEVRON_RIGHT)
                            .w(px(14.))
                            .h(px(14.))
                            .text_color(theme::text_dim())
                            .when(node.expanded, |icon| {
                                icon.with_transformation(Transformation::rotate(radians(
                                    std::f32::consts::FRAC_PI_2,
                                )))
                            })
                            .into_any_element()
                    } else if is_dir {
                        div().w(px(14.)).h(px(14.)).into_any_element()
                    } else {
                        svg()
                            .path(assets::ICON_FILE)
                            .w(px(14.))
                            .h(px(14.))
                            .text_color(theme::text_dim())
                            .into_any_element()
                    },
                )
                .when(is_dir, |row| {
                    row.child(
                        svg()
                            .path(if node.expanded {
                                assets::ICON_FOLDER_OPEN
                            } else {
                                assets::ICON_FOLDER
                            })
                            .w(px(14.))
                            .h(px(14.))
                            .text_color(theme::text_dim()),
                    )
                })
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .text_color(theme::text())
                        .child(node.entry.name.clone()),
                )
                .when(node.loading, |row| {
                    row.child(
                        div()
                            .text_xs()
                            .text_color(theme::text_dim())
                            .child("…"),
                    )
                })
                .when(show_details && !is_dir, |row| {
                    row.child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(theme::text_dim())
                            .child(format_size(node.entry.size)),
                    )
                })
                .into_any_element(),
        );
        if node.expanded {
            // A create editor targeting this directory goes before its
            // children.
            if let Some(ed) = editor.filter(|ed| ed.target.is_none() && ed.parent == node.entry.path)
            {
                rows.push(tree_editor_row(ed, depth + 1, ed.is_dir));
            }
            if let Some(children) = node.children.as_ref() {
                render_tree_rows(
                    children,
                    depth + 1,
                    selected,
                    drag_highlight,
                    show_details,
                    session,
                    session_id,
                    temp_download_cache,
                    editor,
                    cx,
                    rows,
                );
            }
        }
    }
}

/// The inline rename/create row: a text field in tree clothing. Enter and
/// Escape are handled by the tree container's ConfirmDelete/CancelDelete
/// action handlers (the FileTree key bindings dispatch there while the
/// field is focused).
fn tree_editor_row(
    editor: &TreeEditor,
    depth: usize,
    is_dir: bool,
) -> gpui::AnyElement {
    div()
        .id("tree-editor-row")
        .h(px(24.))
        .ml(px(4. + depth as f32 * 14.))
        .mr(px(4.))
        .px(px(6.))
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .rounded_sm()
        .bg(theme::selection())
        .child(
            svg()
                .path(if is_dir {
                    assets::ICON_FOLDER
                } else {
                    assets::ICON_FILE
                })
                .w(px(14.))
                .h(px(14.))
                .text_color(theme::text_dim())
                .into_any_element(),
        )
        .child(editor.field.clone())
        .into_any_element()
}

impl Focusable for RootView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A drag that ends outside the tree (no drop) leaves no event behind;
        // heal stale drag-hover state once gpui reports the drag is over.
        if self.tree_drag_target.is_some() && !cx.has_active_drag() {
            self.tree_drag_target = None;
            self.tree_dragging = None;
        }
        // Deferred focus requests: session events arrive without a `Window`,
        // so they set a flag that is applied here on the next frame.
        let active = self.active;
        if let Some(tab) = self.tabs.get_mut(active) {
            if tab.terminal_focus_pending {
                tab.terminal_focus_pending = false;
                let focus_handle = tab.focus_handle.clone();
                window.focus(&focus_handle, cx);
            }
        }
        let mut root = div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme::bg())
            .text_color(theme::text())
            .text_size(px(13.))
            .font_family(theme::FONT_UI)
            // Fallback for Escape/Enter while the delete confirmation is up
            // and the file tree does not have focus (a handled FileTree key
            // binding never reaches this listener).
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                if this.help_open && event.keystroke.key.as_str() == "escape" {
                    this.help_open = false;
                    cx.notify();
                    return;
                }
                if this.theme_menu && event.keystroke.key.as_str() == "escape" {
                    this.theme_menu = false;
                    cx.notify();
                    return;
                }
                if this.confirm_delete.is_none() {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "escape" => {
                        this.confirm_delete = None;
                        cx.notify();
                    }
                    "enter" => {
                        let remote = this.confirm_delete.clone();
                        if let Some(remote) = remote {
                            this.delete_remote_confirmed(remote, cx);
                        }
                    }
                    _ => {}
                }
            }))
            .child(self.render_header(cx))
            .child(self.render_tab_bar(cx));

        if let Some(form) = self.form.as_ref() {
            root = root.child(Self::render_form(form, cx));
        }

        root = root
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.))
                    .child(self.render_sidebar(cx))
                    .child(self.render_split_handle(cx))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .child(self.render_log_toolbar(cx))
                            .child(self.render_terminal(cx).into_any_element()),
                    ),
            )
            .child(self.render_statusbar(cx));

        // Theme switcher dropdown, anchored under the header: a transparent
        // layer to dismiss, then the menu (same pattern as the context menu).
        if self.theme_menu {
            let active = theme::active_name();
            let themes = theme::list();
            let mut menu = div()
                .id("theme-menu")
                .absolute()
                .top(px(HEADER_HEIGHT))
                .right(px(8.))
                .w(px(200.))
                .max_h(px(400.))
                .overflow_y_scroll()
                .bg(theme::panel())
                .border_1()
                .border_color(theme::border())
                .rounded_md()
                .p_1()
                .flex()
                .flex_col()
                .shadow_md();
            let mut last_appearance = None;
            for (ix, (name, appearance)) in themes.into_iter().enumerate() {
                if last_appearance != Some(appearance) {
                    last_appearance = Some(appearance);
                    menu = menu.child(
                        div()
                            .px_2()
                            .pt_1()
                            .pb_0p5()
                            .text_xs()
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(theme::text_dim())
                            .child(match appearance {
                                theme::Appearance::Dark => "Dark",
                                theme::Appearance::Light => "Light",
                            }),
                    );
                }
                let is_active = name == active;
                let row_name = name.clone();
                menu = menu.child(
                    div()
                        .id(("theme-item", ix))
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .text_xs()
                        .text_color(if is_active {
                            theme::accent()
                        } else {
                            theme::text()
                        })
                        .hover(|item| item.bg(theme::selection()))
                        .child(format!("{} {}", if is_active { "✓" } else { " " }, name))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.apply_theme(row_name.clone(), cx);
                        })),
                );
            }
            root = root
                .child(
                    div()
                        .id("theme-menu-dismiss")
                        .absolute()
                        .top_0()
                        .right_0()
                        .bottom_0()
                        .left_0()
                        .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                            this.theme_menu = false;
                            cx.notify();
                        }))
                        .on_mouse_down(MouseButton::Right, cx.listener(|this, _, _, cx| {
                            this.theme_menu = false;
                            cx.notify();
                        })),
                )
                .child(
                    // Swallow presses that start inside the menu so they
                    // never reach the dismiss layer below: otherwise the
                    // dismiss closes the menu between the row's mouse-down
                    // and mouse-up, and the row's click never completes.
                    menu.on_mouse_down(MouseButton::Left, cx.listener(|_, _, _, cx| {
                        cx.stop_propagation();
                    }))
                    .on_mouse_down(MouseButton::Right, cx.listener(|_, _, _, cx| {
                        cx.stop_propagation();
                    })),
                );
        }

        // Right-click context menu from the file tree, painted above
        // everything else: a transparent layer to dismiss, then the menu.
        // Directories offer the recursive ZIP download; files the full
        // action set.
        if let Some((position, path, is_dir)) = self.context_menu.clone() {
            let mut menu = div()
                .id("context-menu")
                .absolute()
                .left(position.x)
                .top(position.y)
                .min_w(px(160.))
                .bg(theme::panel())
                .border_1()
                .border_color(theme::border())
                .rounded_md()
                .p_1()
                .flex()
                .flex_col()
                .shadow_md();
            if is_dir {
                menu = menu.child(
                    div()
                        .id("context-menu-zip")
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .text_xs()
                        .text_color(theme::text())
                        .hover(|item| item.bg(theme::selection()))
                        .child(format!(
                            "Download {} as ZIP",
                            path.file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_else(|| path.display().to_string())
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.context_menu = None;
                            this.download_zip_selected(cx);
                        })),
                );
            } else {
                // The tail -f closure captures `path` by move; the other
                // items get their own clone.
                let vscode_path = path.clone();
                let edit_path = path.clone();
                let file_name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                let delete_path = path.clone();
                let rename_path = path.clone();
                menu = menu
                    .child(
                        div()
                            .id("context-menu-tail")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_xs()
                            .text_color(theme::text())
                            .hover(|item| item.bg(theme::selection()))
                            .child(format!(
                                "tail -f {}",
                                path.file_name()
                                    .map(|name| name.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| path.display().to_string())
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_log_tab(path.clone(), cx);
                            })),
                    )
                    .child(
                        div()
                            .id("context-menu-download")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_xs()
                            .text_color(theme::text())
                            .hover(|item| item.bg(theme::selection()))
                            .child("Download")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.context_menu = None;
                                this.download_selected(cx);
                            })),
                    )
                    .child(
                        div()
                            .id("context-menu-edit")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_xs()
                            .text_color(theme::text())
                            .hover(|item| item.bg(theme::selection()))
                            .child(format!("Edit {file_name}"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.start_remote_edit(edit_path.clone(), EditorChoice::Default, cx);
                            })),
                    )
                    .child(
                        div()
                            .id("context-menu-vscode")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_xs()
                            .text_color(theme::text())
                            .hover(|item| item.bg(theme::selection()))
                            .child("Open in VS Code")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.start_remote_edit(vscode_path.clone(), EditorChoice::VsCode, cx);
                            })),
                    )
                    .child(
                        div()
                            .id("context-menu-rename")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_xs()
                            .text_color(theme::text())
                            .hover(|item| item.bg(theme::selection()))
                            .child("Rename")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.context_menu = None;
                                this.start_rename(rename_path.clone(), window, cx);
                            })),
                    )
                    .child(
                        div()
                            .id("context-menu-delete")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_xs()
                            .text_color(theme::danger())
                            .hover(|item| item.bg(theme::selection()))
                            .child("Delete")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.context_menu = None;
                                this.ask_delete(delete_path.clone(), cx);
                            })),
                    );
            }
            root = root
                .child(
                    div()
                        .id("context-menu-dismiss")
                        .absolute()
                        .top_0()
                        .right_0()
                        .bottom_0()
                        .left_0()
                        .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                            this.context_menu = None;
                            cx.notify();
                        }))
                        .on_mouse_down(MouseButton::Right, cx.listener(|this, _, _, cx| {
                            this.context_menu = None;
                            cx.notify();
                        })),
                )
                .child(
                    // Swallow presses that start inside the menu so they
                    // never reach the dismiss layer (same race as the theme
                    // menu: the dismiss would close the menu between a
                    // row's mouse-down and mouse-up).
                    menu.on_mouse_down(MouseButton::Left, cx.listener(|_, _, _, cx| {
                        cx.stop_propagation();
                    }))
                    .on_mouse_down(MouseButton::Right, cx.listener(|_, _, _, cx| {
                        cx.stop_propagation();
                    })),
                );
        }

        // Help overlay: shortcuts & features, centered and scrollable.
        // Escape or a click outside the panel closes it.
        if self.help_open {
            let mut panel = div()
                .id("help-panel")
                .w(px(540.))
                // Fixed (not max) height: the overflow scrollbar must engage
                // whenever the content exceeds it — with content-sized
                // max_h some layouts never became scrollable.
                .h(px(560.))
                .overflow_y_scroll()
                .bg(theme::panel())
                .border_1()
                .border_color(theme::border())
                .rounded_lg()
                .shadow_md()
                .p_3()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(theme::accent())
                                .child("aetherium — shortcuts & features"),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme::text_dim())
                                .child("Esc to close"),
                        ),
                );
            for section in HELP_SECTIONS {
                let mut block = div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .pb_1()
                            .text_sm()
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(theme::text())
                            .child(section.title),
                    );
                for (keys, description) in section.rows {
                    block = block.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_baseline()
                            .gap_3()
                            .py_0p5()
                            .child(
                                div()
                                    .w(px(210.))
                                    .flex_none()
                                    .font_family(theme::FONT_MONO)
                                    .text_xs()
                                    .text_color(theme::accent())
                                    .child(*keys),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .text_xs()
                                    .text_color(theme::text_dim())
                                    .child(*description),
                            ),
                    );
                }
                panel = panel.child(block);
            }
            root = root
                .child(
                    div()
                        .id("help-overlay")
                        .absolute()
                        .top_0()
                        .right_0()
                        .bottom_0()
                        .left_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                            this.help_open = false;
                            cx.notify();
                        }))
                        .on_mouse_down(MouseButton::Right, cx.listener(|this, _, _, cx| {
                            this.help_open = false;
                            cx.notify();
                        }))
                        .child(
                            // Swallow presses inside the panel so they don't
                            // hit the dismiss layer (same race as the menus).
                            panel
                                .on_mouse_down(MouseButton::Left, cx.listener(|_, _, _, cx| {
                                    cx.stop_propagation();
                                }))
                                .on_mouse_down(MouseButton::Right, cx.listener(|_, _, _, cx| {
                                    cx.stop_propagation();
                                })),
                        ),
                );
        }

        // Delete confirmation dialog, painted above everything (like the
        // context menu). Clicking anywhere outside the panel, Escape or the
        // Cancel button dismisses; the Delete button (or Enter) confirms.
        // The panel swallows mouse-downs so button clicks don't bubble to
        // the overlay and cancel the dialog mid-press.
        if let Some(remote) = self.confirm_delete.clone() {
            let name = remote
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| remote.display().to_string());
            root = root.child(
                div()
                    .id("delete-dialog")
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .left_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(gpui::black().opacity(0.4))
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                        this.confirm_delete = None;
                        cx.notify();
                    }))
                    .on_mouse_down(MouseButton::Right, cx.listener(|this, _, _, cx| {
                        this.confirm_delete = None;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .id("delete-dialog-panel")
                            .w(px(320.))
                            .bg(theme::panel())
                            .border_1()
                            .border_color(theme::border())
                            .rounded_md()
                            .p_4()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .shadow_md()
                            .on_mouse_down(MouseButton::Left, cx.listener(|_, _, _, cx| {
                                cx.stop_propagation();
                            }))
                            .on_mouse_down(MouseButton::Right, cx.listener(|_, _, _, cx| {
                                cx.stop_propagation();
                            }))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme::text())
                                    .child(format!("Delete “{name}” permanently?")),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::text_dim())
                                    .child("This cannot be undone. Directories go recursively."),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_2()
                                    .justify_end()
                                    .child(
                                        div()
                                            .id("delete-dialog-cancel")
                                            .px_3()
                                            .py_1()
                                            .rounded_sm()
                                            .cursor_pointer()
                                            .text_xs()
                                            .border_1()
                                            .border_color(theme::border())
                                            .text_color(theme::text())
                                            .hover(|button| button.bg(theme::hover()))
                                            .child("Cancel")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.confirm_delete = None;
                                                cx.notify();
                                            })),
                                    )
                                    .child({
                                        let remote = remote.clone();
                                        div()
                                            .id("delete-dialog-confirm")
                                            .px_3()
                                            .py_1()
                                            .rounded_sm()
                                            .cursor_pointer()
                                            .text_xs()
                                            .bg(theme::danger())
                                            .text_color(gpui::white())
                                            .hover(|button| button.opacity(0.9))
                                            .child("Delete")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.delete_remote_confirmed(remote.clone(), cx);
                                            }))
                                    }),
                            ),
                    ),
            );
        }

        // "Sync back to the device?" for a locally edited remote file
        // (MobaXterm style). Clicking outside, Escape or "Ignore" keeps the
        // local changes unsynced; "Upload" pushes the temp copy back over the
        // shared transfer path.
        if let Some(ask) = self.edit_sync_ask.clone() {
            let name = ask
                .remote
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| ask.remote.display().to_string());
            root = root.child(
                div()
                    .id("edit-sync-dialog")
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .left_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(gpui::black().opacity(0.4))
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                        this.answer_edit_sync(false, cx);
                    }))
                    .on_mouse_down(MouseButton::Right, cx.listener(|this, _, _, cx| {
                        this.answer_edit_sync(false, cx);
                    }))
                    .child(
                        div()
                            .id("edit-sync-panel")
                            .w(px(360.))
                            .bg(theme::panel())
                            .border_1()
                            .border_color(theme::border())
                            .rounded_md()
                            .p_4()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .shadow_md()
                            .on_mouse_down(MouseButton::Left, cx.listener(|_, _, _, cx| {
                                cx.stop_propagation();
                            }))
                            .on_mouse_down(MouseButton::Right, cx.listener(|_, _, _, cx| {
                                cx.stop_propagation();
                            }))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme::text())
                                    .child(format!("“{name}” changed on disk.")),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::text_dim())
                                    .child(format!(
                                        "Upload the changes back to {}?",
                                        ask.target
                                    )),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_2()
                                    .justify_end()
                                    .child(
                                        div()
                                            .id("edit-sync-ignore")
                                            .px_3()
                                            .py_1()
                                            .rounded_sm()
                                            .cursor_pointer()
                                            .text_xs()
                                            .border_1()
                                            .border_color(theme::border())
                                            .text_color(theme::text())
                                            .hover(|button| button.bg(theme::hover()))
                                            .child("Ignore")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.answer_edit_sync(false, cx);
                                            })),
                                    )
                                    .child(
                                        div()
                                            .id("edit-sync-upload")
                                            .px_3()
                                            .py_1()
                                            .rounded_sm()
                                            .cursor_pointer()
                                            .text_xs()
                                            .bg(theme::accent())
                                            .text_color(theme::bg())
                                            .hover(|button| button.opacity(0.9))
                                            .child("Upload")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.answer_edit_sync(true, cx);
                                            })),
                                    ),
                            ),
                    ),
            );
        }

        root
    }
}

// Actions bound in `main.rs` under the "FileTree" key context. DeleteEntry
// asks to delete the selected entry; while the confirmation is up, Enter
// confirms and Escape cancels. RenameEntry opens the inline rename editor.
// Handlers live on the file-tree container.
gpui::actions!(file_tree, [DeleteEntry, CancelDelete, ConfirmDelete, RenameEntry]);

#[cfg(test)]
// [utest->req~uncolored-cell-coloring~1]
mod tests {
    use super::*;

    #[test]
    fn uncolored_cells_are_detected() {
        // A cell exactly as the terminal emulator defaults it: the program
        // printed plain text, no SGR styling.
        assert!(cell_is_uncolored(&Cell::default()));

        let mut fg = Cell::default();
        fg.fg = Color::Named(NamedColor::Red);
        assert!(!cell_is_uncolored(&fg));

        let mut bg = Cell::default();
        bg.bg = Color::Named(NamedColor::Blue);
        assert!(!cell_is_uncolored(&bg));

        let mut indexed = Cell::default();
        indexed.fg = Color::Indexed(196);
        assert!(!cell_is_uncolored(&indexed));

        let mut inverse = Cell::default();
        inverse.flags.insert(Flags::INVERSE);
        assert!(!cell_is_uncolored(&inverse));
    }

    // [utest->req~help-shortcut-list~1]
    #[test]
    fn help_sections_are_well_formed() {
        assert!(!HELP_SECTIONS.is_empty());
        for section in HELP_SECTIONS {
            assert!(!section.title.is_empty());
            assert!(
                !section.rows.is_empty(),
                "section {} lists no shortcuts",
                section.title
            );
            for (keys, description) in section.rows {
                assert!(!keys.is_empty() && !description.is_empty());
            }
        }
    }
}
