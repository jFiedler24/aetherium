//! Root view: Zed-style layout with a header bar, a sidebar (profiles +
//! remote file tree), the terminal canvas, and a status bar.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};
use gpui::{
    App, Bounds, Context, CursorStyle, DragMoveEvent, Entity, ExternalPaths, FocusHandle,
    Focusable, Hsla, KeyDownEvent, MouseButton, MouseDownEvent, Pixels, Point, ScrollHandle,
    ScrollWheelEvent, ShapedLine, SharedString, TextRun, Transformation, UnderlineStyle, Window,
    canvas, div, fill, font, point, prelude::*, px, radians, rgb, rgba, size, svg,
};
use parking_lot::Mutex;

use crate::assets;
use crate::log_highlight::LogHighlighter;
use crate::profiles::{AuthMethod, Profile, ProfileStore};
use crate::recents::{RecentEntry, RecentStore, now_unix, relative_time};
use crate::session::{Command as SessionCommand, Event as SessionEvent, FileEntry, SessionHandle};
use crate::terminal_model::TerminalModel;
use crate::text_field::TextField;
use crate::theme;

const TERMINAL_FONT_SIZE: f32 = 13.0;
const TERMINAL_LINE_HEIGHT: f32 = TERMINAL_FONT_SIZE * 1.35;
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
                                assets::ICON_CHEVRON_RIGHT
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
    /// Active file transfer: (label, done bytes, total bytes, bytes/sec, eta seconds).
    transfer: Option<(String, u64, u64, f64, u64)>,
    /// Remote directories that pending uploads write into; refreshed every
    /// time a transfer completes.
    pending_upload_dirs: Vec<PathBuf>,
    /// Log highlighting for `tail -f` tabs; `None` for shell tabs.
    highlighter: Option<Arc<LogHighlighter>>,
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

/// Which auth method the profile form is editing.
#[derive(Clone, Copy, PartialEq)]
enum AuthKind {
    Password,
    KeyFile,
    Agent,
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
    context_menu: Option<(Point<Pixels>, PathBuf)>,
    /// Entry under the cursor during an internal file-tree drag.
    tree_drag_target: Option<TreeDragTarget>,
    /// Entry being dragged in the file tree (set when a drag starts moving).
    tree_dragging: Option<DraggedEntry>,
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
    terminal_wake_tx: std_mpsc::Sender<()>,
    focus_handle: FocusHandle,
}

impl RootView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (terminal_wake_tx, terminal_wake_rx) = std_mpsc::channel::<()>();
        let view = Self {
            store: ProfileStore::load(),
            selected: None,
            form: None,
            recents: RecentStore::load(),
            status: "not connected".to_string(),
            tabs: Vec::new(),
            active: 0,
            next_tab_id: 0,
            context_menu: None,
            tree_drag_target: None,
            tree_dragging: None,
            sidebar_tab: SidebarTab::Sessions,
            sidebar_width: px(260.),
            show_file_details: false,
            logs: Vec::new(),
            log_scroll: ScrollHandle::default(),
            logs_scroll_pending: false,
            terminal_wake_tx,
            focus_handle: cx.focus_handle(),
        };
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
    fn spawn_repaint_loop(&self, cx: &mut Context<Self>, wake_rx: std_mpsc::Receiver<()>) {
        cx.spawn(async move |this, cx| {
            loop {
                // Wait for a terminal wake or the 16ms fallback tick, whichever
                // comes first.
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
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
                    if this.tabs.iter().any(|tab| tab.terminal.take_dirty()) {
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
                            this.handle_tab_event(index, event);
                            handled = true;
                        }
                    }
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

    fn handle_tab_event(&mut self, index: usize, event: SessionEvent) {
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
            SessionEvent::Error(message) => {
                let tab = &mut self.tabs[index];
                tab.status = message.clone();
                if tab.state == ConnState::Connecting {
                    tab.state = ConnState::Disconnected;
                }
                let tab_label = Self::tab_log_label(&self.tabs[index]);
                self.log(LogLevel::Error, format!("{tab_label}: {message}"));
            }
            SessionEvent::Disconnected => {
                let tab_id = self.tabs[index].id;
                let tab = &mut self.tabs[index];
                tab.state = ConnState::Disconnected;
                tab.status = "disconnected".to_string();
                tab.tree.clear();
                tab.root_path = None;
                tab.tree_selection = None;
                tab.transfer = None;
                tab.pending_upload_dirs.clear();
                tab.connecting_profile = None;
                self.tree_drag_target = None;
                self.tree_dragging = None;
                // Drop the old session's output; focus stays on the sidebar.
                tab.terminal.reset();
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

    fn connect_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.selected.and_then(|i| self.store.profiles.get(i)).cloned()
        else {
            self.status = "select a profile first".into();
            cx.notify();
            return;
        };
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
            self.status = format!("enter credentials for {}", profile.summary());
            self.form = Some(ProfileForm::from_profile(editing, &profile, cx));
            self.focus_first_form_field(window, cx);
            cx.notify();
            return;
        }
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
            session_id: 1,
            terminal_focus_pending: false,
            geometry: Arc::new(Mutex::new(None)),
            connecting_profile: Some(profile.clone()),
            tree_selection: None,
            transfer: None,
            pending_upload_dirs: Vec::new(),
            highlighter: None,
        };
        session.connect(profile);
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.context_menu = None;
        cx.notify();
    }

    /// Open a new read-only tab that follows a remote file via `tail -f`,
    /// running on the active (parent) tab's connection.
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
            session_id: 0,
            terminal_focus_pending: true,
            geometry: Arc::new(Mutex::new(None)),
            connecting_profile: None,
            tree_selection: None,
            transfer: None,
            pending_upload_dirs: Vec::new(),
            highlighter: Some(Arc::new(LogHighlighter::load())),
        };
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.context_menu = None;
        cx.notify();
    }

    /// Activate a tab, focusing its terminal on the next frame.
    fn activate_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        self.active = index;
        self.context_menu = None;
        self.tree_drag_target = None;
        self.tree_dragging = None;
        self.tabs[index].terminal_focus_pending = true;
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
        let Some(tab) = self.active_tab() else {
            return;
        };
        let Some(session) = tab.session.as_ref() else {
            return;
        };
        for local in paths {
            session.upload(tab.session_id, local.clone(), remote_dir.clone());
        }
        let tab = self.active_tab_mut().expect("checked above");
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
        if let Some(session) = tab.session.as_ref() {
            session.download(tab.session_id, remote);
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
            window.focus(&handle);
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

    fn on_terminal_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let terminal = tab.terminal.clone();
        let is_log = tab.is_log();
        let keystroke = &event.keystroke;
        let mods = &keystroke.modifiers;

        // Scrollback navigation does not go to the PTY.
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
        }

        // Log-follow tabs are read-only views of `tail -f` output.
        if is_log {
            return;
        }

        // Paste: ctrl-shift-v (Linux) or platform-v (macOS).
        let paste = keystroke.key == "v"
            && ((mods.control && mods.shift) || mods.platform);
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
                if let Some(session) = tab.session.as_ref() {
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
            if let Some(session) = tab.session.as_ref() {
                session.input(bytes);
            }
            self.scroll_to_bottom();
        }
        cx.notify();
    }

    /// Jump the display back to the live edge (used whenever input is sent).
    fn scroll_to_bottom(&self) {
        if let Some(tab) = self.active_tab() {
            tab.terminal.term.lock().scroll_display(Scroll::Bottom);
            tab.terminal.mark_dirty();
        }
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
                let already_at_root = dragged.path.parent() == Some(root.as_path());
                return (!already_at_root).then_some(root);
            }
            TreeDragTarget::Row { path, is_dir } => (path.clone(), *is_dir),
        };
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

/// Zed default-dark terminal palette: the `terminal_ansi_*` colors of Zed's
/// built-in theme (MIT-licensed), transcribed from Zed's color scales. The
/// black/white scales are alpha ramps that Zed composites over the terminal
/// background; the alpha is kept so the rendering matches. Normal colors are
/// scale step 11, bright colors step 10, dim colors step 9.
const ANSI_NORMAL: [u32; 8] = [
    0x000000f2, // black
    0xff9592ff, // red
    0x3dd68cff, // green
    0xf5e147ff, // yellow
    0x70b8ffff, // blue
    0xbaa7ffff, // magenta
    0x4ccce6ff, // cyan
    0xeeeeecff, // white
];
const ANSI_BRIGHT: [u32; 8] = [
    0x000000e6, // bright black
    0xec5d5eff, // bright red
    0x33b074ff, // bright green
    0xffff57ff, // bright yellow
    0x3b9effff, // bright blue
    0x7d66d9ff, // bright magenta
    0x23afd0ff, // bright cyan
    0xb5b3adff, // bright white
];
const ANSI_DIM: [u32; 8] = [
    0x000000cc, // dim black
    0xe5484dff, // dim red
    0x30a46cff, // dim green
    0xffe629ff, // dim yellow
    0x0090ffff, // dim blue
    0x6e56cfff, // dim magenta
    0x00a2c7ff, // dim cyan
    0x7c7b74ff, // dim white
];
/// Default terminal foreground (`terminal_foreground`, white scale step 12).
const TERM_FG: u32 = 0xfffffff2;
/// Default terminal background (`terminal_background` = theme background).
const TERM_BG: u32 = 0x22252bff;

fn hex(value: u32) -> Hsla {
    rgb(value).into()
}

fn rgb_to_hsla(color: Rgb) -> Hsla {
    rgb((color.r as u32) << 16 | (color.g as u32) << 8 | color.b as u32).into()
}

/// 256-color lookup: 0-15 palette, 16-231 cube, 232-255 grayscale.
fn indexed_color(index: u8) -> Hsla {
    match index {
        0..=7 => rgba(ANSI_NORMAL[index as usize]).into(),
        8..=15 => rgba(ANSI_BRIGHT[index as usize - 8]).into(),
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
    fn palette(colors: &[u32; 8], index: usize, dim: bool) -> Hsla {
        rgba(if dim { ANSI_DIM[index] } else { colors[index] }).into()
    }
    match color {
        NamedColor::Foreground => {
            rgba(if dim { 0xffffffcc } else { TERM_FG }).into()
        }
        NamedColor::Background => rgba(TERM_BG).into(),
        NamedColor::BrightForeground => rgba(0xffffffe6).into(),
        NamedColor::Black => palette(&ANSI_NORMAL, 0, dim),
        NamedColor::Red => palette(&ANSI_NORMAL, 1, dim),
        NamedColor::Green => palette(&ANSI_NORMAL, 2, dim),
        NamedColor::Yellow => palette(&ANSI_NORMAL, 3, dim),
        NamedColor::Blue => palette(&ANSI_NORMAL, 4, dim),
        NamedColor::Magenta => palette(&ANSI_NORMAL, 5, dim),
        NamedColor::Cyan => palette(&ANSI_NORMAL, 6, dim),
        NamedColor::White => palette(&ANSI_NORMAL, 7, dim),
        NamedColor::BrightBlack => palette(&ANSI_BRIGHT, 0, false),
        NamedColor::BrightRed => palette(&ANSI_BRIGHT, 1, false),
        NamedColor::BrightGreen => palette(&ANSI_BRIGHT, 2, false),
        NamedColor::BrightYellow => palette(&ANSI_BRIGHT, 3, false),
        NamedColor::BrightBlue => palette(&ANSI_BRIGHT, 4, false),
        NamedColor::BrightMagenta => palette(&ANSI_BRIGHT, 5, false),
        NamedColor::BrightCyan => palette(&ANSI_BRIGHT, 6, false),
        NamedColor::BrightWhite => palette(&ANSI_BRIGHT, 7, false),
        NamedColor::DimBlack => rgba(ANSI_DIM[0]).into(),
        NamedColor::DimRed => rgba(ANSI_DIM[1]).into(),
        NamedColor::DimGreen => rgba(ANSI_DIM[2]).into(),
        NamedColor::DimYellow => rgba(ANSI_DIM[3]).into(),
        NamedColor::DimBlue => rgba(ANSI_DIM[4]).into(),
        NamedColor::DimMagenta => rgba(ANSI_DIM[5]).into(),
        NamedColor::DimCyan => rgba(ANSI_DIM[6]).into(),
        NamedColor::DimWhite => rgba(ANSI_DIM[7]).into(),
        // Cursor color and dim/bright foreground/background variants fall
        // back to the defaults.
        _ => rgba(TERM_FG).into(),
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
    cursor: Option<gpui::PaintQuad>,
}

/// Collect styled runs for every visible row of the terminal.
fn collect_runs(
    terminal: &TerminalModel,
    highlighter: Option<&LogHighlighter>,
) -> (Vec<Vec<RowRun>>, Option<(usize, usize, CursorShape)>) {
    let term = terminal.term.lock();
    let content = term.renderable_content();
    let screen_lines = term.screen_lines();
    let cursor = if content.mode.contains(TermMode::SHOW_CURSOR) && content.display_offset == 0 {
        Some((
            content.cursor.point.line.0.max(0) as usize,
            content.cursor.point.column.0,
            content.cursor.shape,
        ))
    } else {
        None
    };

    // For log highlighting: rebuild each visual row's text, then precompute
    // per-row segments as char ranges with their colors.
    let mut row_highlights: Vec<Vec<(usize, usize, Hsla, bool)>> =
        (0..screen_lines).map(|_| Vec::new()).collect();
    if highlighter.is_some() {
        let mut row_texts: Vec<String> = (0..screen_lines).map(|_| String::new()).collect();
        let text_content = term.renderable_content();
        for indexed in text_content.display_iter {
            let line = indexed.point.line.0;
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
    for indexed in content.display_iter {
        let cell: &Cell = indexed.cell;
        let line = indexed.point.line.0;
        if line < 0 || line as usize >= screen_lines {
            continue;
        }
        let flags = cell.flags & STYLE_FLAGS;
        // Spacer cells hold no text; the wide glyph of the previous cell
        // already covers their column.
        let is_spacer = cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER);
        let row = &mut rows[line as usize];
        let col = indexed.point.column.0;
        let line = line as usize;
        let char_ix = row_char_ix[line];
        if !is_spacer {
            row_char_ix[line] += 1;
        }
        let (mut fg, bg) = cell_colors(cell);
        let mut bold = flags.contains(Flags::BOLD);
        if let Some(segments) = row_highlights.get(line) {
            if let Some((_, _, highlight_fg, highlight_bold)) = segments
                .iter()
                .find(|(start, end, _, _)| char_ix >= *start && char_ix < *end)
            {
                fg = *highlight_fg;
                bold |= *highlight_bold;
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

        let mergeable = row.last().is_some_and(|last| {
            last.bold == bold
                && last.italic == italic
                && last.strikethrough == strikethrough
                && last.underline == underline
                && colors_equal(last.fg, fg)
                && colors_equal(last.bg, bg)
                && last.start_col + last.span_cols == col
        });

        if mergeable {
            let last = row.last_mut().unwrap();
            last.span_cols += 1;
            if !is_spacer {
                last.text.push(cell.c);
            }
        } else {
            row.push(RowRun {
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
    (rows, cursor)
}

fn colors_equal(a: Hsla, b: Hsla) -> bool {
    a == b
}

impl RootView {
    fn render_terminal(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(tab) = self.tabs.get(self.active) else {
            return div()
                .flex_1()
                .min_w(px(0.))
                .h_full()
                .bg(hex(TERM_BG))
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
        let highlighter = tab.highlighter.clone();

        div()
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .bg(hex(TERM_BG))
            .track_focus(&focus_handle)
            .on_key_down(cx.listener(Self::on_terminal_key_down))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _window, cx| {
                // Positive y is wheel-up; alacritty scrolls into the
                // scrollback history for positive deltas, so pass it through.
                // `Scroll::Delta` is whole lines (i32); approximate the pixel
                // delta against the cell height.
                let pixel_delta = event.delta.pixel_delta(px(TERMINAL_LINE_HEIGHT));
                let lines = (f32::from(pixel_delta.y) / TERMINAL_LINE_HEIGHT).round() as i32;
                if lines != 0 {
                    if let Some(tab) = this.active_tab() {
                        tab.terminal.term.lock().scroll_display(Scroll::Delta(lines));
                        tab.terminal.mark_dirty();
                        cx.notify();
                    }
                }
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| {
                if let Some(tab) = this.active_tab() {
                    window.focus(&tab.focus_handle);
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
                    move |bounds, window, _cx| {
                        terminal_prepaint(
                            bounds,
                            window,
                            &terminal,
                            &geometry,
                            highlighter.as_deref(),
                        )
                    },
                    move |_bounds, prepaint, window, cx| {
                        for quad in prepaint.backgrounds {
                            window.paint_quad(quad);
                        }
                        for (origin, line) in prepaint.lines {
                            let line_height = prepaint_line_height();
                            let _ = line.paint(origin, line_height, window, cx);
                        }
                        if let Some(cursor) = prepaint.cursor {
                            window.paint_quad(cursor);
                        }
                    },
                )
                .size_full(),
            )
    }
}

fn prepaint_line_height() -> Pixels {
    px(TERMINAL_LINE_HEIGHT)
}

fn terminal_prepaint(
    bounds: Bounds<Pixels>,
    window: &mut Window,
    terminal: &TerminalModel,
    geometry: &Arc<Mutex<Option<TermGeometry>>>,
    highlighter: Option<&LogHighlighter>,
) -> TerminalPrepaint {
    let font = font(theme::FONT_MONO);
    let font_size = px(TERMINAL_FONT_SIZE);
    let line_height = prepaint_line_height();

    // Measure a monospace cell from a probe string.
    let probe = "0000000000";
    let probe_line = window.text_system().shape_line(
        probe.into(),
        font_size,
        &[TextRun {
            len: probe.len(),
            font: font.clone(),
            color: hex(TERM_FG),
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    );
    let cell_width = (probe_line.width / probe.len() as f32).max(px(1.));

    *geometry.lock() = Some(TermGeometry {
        bounds,
        cell_width,
        line_height,
    });

    let (rows, cursor) = collect_runs(terminal, highlighter);
    let default_bg = hex(TERM_BG);

    let mut lines = Vec::new();
    let mut backgrounds = Vec::new();

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

    let cursor = cursor.map(|(line, col, shape)| {
        let origin = point(
            bounds.left() + cell_width * col as f32,
            bounds.top() + line_height * line as f32,
        );
        let cursor_bounds = match shape {
            CursorShape::Underline => Bounds::new(
                point(origin.x, origin.y + line_height - px(2.)),
                size(cell_width, px(2.)),
            ),
            CursorShape::Beam => Bounds::new(origin, size(px(2.), line_height)),
            _ => Bounds::new(origin, size(cell_width, line_height)),
        };
        fill(cursor_bounds, theme::accent())
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
                            .h(px(20.)),
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
            .child(header_button("new-profile", "+ New", cx, |this, window, cx| {
                this.open_new_profile_form(window, cx)
            }))
            .child(header_button("edit-profile", "Edit", cx, |this, window, cx| {
                this.open_edit_profile_form(window, cx)
            }))
            .child(header_button("delete-profile", "Delete", cx, |this, _window, cx| {
                this.delete_selected_profile(cx)
            }))
            .into_any_element()
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
                    .child(svg().path(assets::ICON_CONFIGURATION).w(px(12.)).h(px(12.)))
                    .child(div().text_xs().text_color(theme::text_dim()).child("PROFILES")),
            )
            .child(self.render_profile_list(cx))
            .child(
                div().px_2().pb_2().child(header_button(
                    "connect",
                    "Connect",
                    cx,
                    |this, window, cx| {
                        this.connect_selected(window, cx);
                    },
                )),
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

    fn render_recents(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let now = now_unix();
        let mut rows = Vec::new();
        for (ix, entry) in self.recents.entries.iter().enumerate() {
            let subtitle = format!("{} · {}", entry.profile_name, relative_time(now, entry.connected_at_unix));
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
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected = Some(ix);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(profile.name.clone())
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::text_dim())
                                    .child(profile.summary()),
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

    fn render_file_tree(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut rows = Vec::new();
        let empty: Vec<TreeNode> = Vec::new();
        let (tree, tree_selection, connected) = match self.active_tab() {
            Some(tab) => (&tab.tree, tab.tree_selection.clone(), tab.state == ConnState::Connected),
            None => (&empty, None, false),
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
            render_tree_rows(
                tree,
                0,
                tree_selection.as_ref(),
                drag_highlight.as_ref(),
                self.show_file_details,
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
                this.drop_tree_entry(dragged.clone(), TreeDragTarget::Background, cx);
            }))
            .on_drag_move::<DraggedEntry>(cx.listener(
                |this, event: &DragMoveEvent<DraggedEntry>, _, cx| {
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

    fn render_statusbar(&mut self, _cx: &mut Context<Self>) -> gpui::AnyElement {
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
        let (state_text, state_color) = match state {
            ConnState::Disconnected => ("○ disconnected", theme::text_dim()),
            ConnState::Connecting => ("◌ connecting…", theme::warning()),
            ConnState::Connected => ("● connected", theme::success()),
        };
        // Active transfer progress, shown between the state and the status
        // message.
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
            .child(div().text_color(state_color).child(state_text))
            .when_some(transfer_text, |bar, text| {
                bar.child(
                    div()
                        .flex_1()
                        .text_color(theme::warning())
                        .truncate()
                        .child(text),
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
fn render_tree_rows(
    nodes: &[TreeNode],
    depth: usize,
    selected: Option<&PathBuf>,
    drag_highlight: Option<&PathBuf>,
    show_details: bool,
    cx: &mut Context<RootView>,
    rows: &mut Vec<gpui::AnyElement>,
) {
    for node in nodes {
        let path = node.entry.path.clone();
        let is_dir = node.entry.is_dir;
        let is_selected = selected == Some(&node.entry.path);
        let highlighted = is_dir && drag_highlight == Some(&node.entry.path);
        let row_path = path.clone();
        let drag_move_path = path.clone();
        let external_drop_path = path.clone();
        let tree_drop_path = path.clone();

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
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_tree_node(path.clone(), cx);
                }))
                // Start dragging this entry; a small label follows the cursor
                // (Zed's project-panel drag image).
                .on_drag(
                    DraggedEntry {
                        path: row_path.clone(),
                        is_dir,
                        name: node.entry.name.clone().into(),
                    },
                    |drag, click_offset, _window, cx| {
                        cx.new(|_| DraggedEntryView {
                            name: drag.name.clone(),
                            is_dir: drag.is_dir,
                            click_offset,
                        })
                    },
                )
                .on_drag_move::<DraggedEntry>(cx.listener(
                    move |this, event: &DragMoveEvent<DraggedEntry>, window, cx| {
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
                // Right-click a file for the context menu (e.g. tail -f).
                .when(!node.entry.is_dir, |row| {
                    let menu_path = node.entry.path.clone();
                    row.on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                            if let Some(tab) = this.active_tab_mut() {
                                tab.tree_selection = Some(menu_path.clone());
                            }
                            this.context_menu = Some((event.position, menu_path.clone()));
                            cx.notify();
                        }),
                    )
                })
                .child(
                    // Leading glyph: disclosure chevron for directories, a
                    // file icon for files (Zed project-panel layout).
                    if is_dir {
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
                    } else {
                        svg()
                            .path(assets::ICON_FILE)
                            .w(px(14.))
                            .h(px(14.))
                            .text_color(theme::text_dim())
                            .into_any_element()
                    },
                )
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
            if let Some(children) = node.children.as_ref() {
                render_tree_rows(children, depth + 1, selected, drag_highlight, show_details, cx, rows);
            }
        }
    }
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
                window.focus(&focus_handle);
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
                    .child(self.render_terminal(cx)),
            )
            .child(self.render_statusbar(cx));

        // Right-click context menu from the file tree, painted above
        // everything else: a transparent layer to dismiss, then the menu.
        if let Some((position, path)) = self.context_menu.clone() {
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
                    div()
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
                        .shadow_md()
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
                        ),
                );
        }

        root
    }
}
