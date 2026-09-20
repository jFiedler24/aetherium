//! Root view: Zed-style layout with a header bar, a sidebar (profiles +
//! remote file tree), the terminal canvas, and a status bar.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};
use gpui::{
    App, Bounds, Context, Entity, ExternalPaths, FocusHandle, Focusable, Hsla, KeyDownEvent,
    MouseButton, MouseDownEvent, Pixels, Point, ScrollWheelEvent, ShapedLine, SharedString,
    TextRun, UnderlineStyle, Window, canvas, div, fill, font, point, prelude::*, px, rgb, size,
    svg,
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
const SIDEBAR_WIDTH: f32 = 260.0;
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
    focus_handle: FocusHandle,
}

impl RootView {
    pub fn new(cx: &mut Context<Self>) -> Self {
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
            focus_handle: cx.focus_handle(),
        };
        view.spawn_repaint_loop(cx);
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

    /// ~60fps dirty-flag poll: repaint only when a terminal changed.
    fn spawn_repaint_loop(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
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
                    .list_dir(session_id, home_dir);
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
                    }
                }
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
                self.tabs[index].transfer = Some((label, 0, 0, 0.0, 0));
            }
            SessionEvent::TransferProgress { label, done_bytes, total_bytes, bytes_per_second, eta_seconds } => {
                self.tabs[index].transfer = Some((label, done_bytes, total_bytes, bytes_per_second, eta_seconds));
            }
            SessionEvent::TransferDone { label } => {
                let tab = &mut self.tabs[index];
                tab.transfer = None;
                tab.status = label;
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
            }
            SessionEvent::Error(message) => {
                let tab = &mut self.tabs[index];
                tab.status = message;
                if tab.state == ConnState::Connecting {
                    tab.state = ConnState::Disconnected;
                }
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
            }
            SessionEvent::TailEnded { tab_id } => {
                self.end_log_tab(tab_id, "tail ended");
            }
            SessionEvent::TailError { tab_id, message } => {
                self.end_log_tab(tab_id, message);
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

    fn connect_selected(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.selected.and_then(|i| self.store.profiles.get(i)).cloned()
        else {
            self.status = "select a profile first".into();
            cx.notify();
            return;
        };
        self.connect_profile(profile, cx);
    }

    /// Open a new shell tab and connect it to `profile`.
    fn connect_profile(&mut self, profile: Profile, cx: &mut Context<Self>) {
        let terminal = TerminalModel::new(80, 24);
        let session = SessionHandle::spawn(terminal.clone());
        let id = self.alloc_tab_id();
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
            self.connect_profile(profile, cx);
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
            self.form = Some(ProfileForm::from_profile(index, &profile, cx));
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
}

impl ProfileForm {
    fn blank(cx: &mut Context<RootView>) -> Self {
        Self::from_fields(cx, None, AuthKind::Password, "", "", "22", "", "", "", "")
    }

    fn from_profile(index: usize, profile: &Profile, cx: &mut Context<RootView>) -> Self {
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
            Some(index),
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

/// Alacritty's default dark palette.
const ANSI_NORMAL: [u32; 8] = [
    0x1d1f21, 0xcc6666, 0xb5bd68, 0xf0c674, 0x81a2be, 0xb294bb, 0x8abeb7, 0xc5c8c6,
];
const ANSI_BRIGHT: [u32; 8] = [
    0x666666, 0xd54e53, 0xb9ca4a, 0xe7c547, 0x7aa6da, 0xc397d8, 0x70c0b1, 0xeaeaea,
];
const TERM_FG: u32 = 0xc5c8c6;
const TERM_BG: u32 = 0x1d1f21;

fn hex(value: u32) -> Hsla {
    rgb(value).into()
}

fn rgb_to_hsla(color: Rgb) -> Hsla {
    rgb((color.r as u32) << 16 | (color.g as u32) << 8 | color.b as u32).into()
}

/// 256-color lookup: 0-15 palette, 16-231 cube, 232-255 grayscale.
fn indexed_color(index: u8) -> Hsla {
    match index {
        0..=7 => hex(ANSI_NORMAL[index as usize]),
        8..=15 => hex(ANSI_BRIGHT[index as usize - 8]),
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
    let value = match color {
        NamedColor::Foreground => TERM_FG,
        NamedColor::Background => TERM_BG,
        NamedColor::Black => ANSI_NORMAL[0],
        NamedColor::Red => ANSI_NORMAL[1],
        NamedColor::Green => ANSI_NORMAL[2],
        NamedColor::Yellow => ANSI_NORMAL[3],
        NamedColor::Blue => ANSI_NORMAL[4],
        NamedColor::Magenta => ANSI_NORMAL[5],
        NamedColor::Cyan => ANSI_NORMAL[6],
        NamedColor::White => ANSI_NORMAL[7],
        NamedColor::BrightBlack => ANSI_BRIGHT[0],
        NamedColor::BrightRed => ANSI_BRIGHT[1],
        NamedColor::BrightGreen => ANSI_BRIGHT[2],
        NamedColor::BrightYellow => ANSI_BRIGHT[3],
        NamedColor::BrightBlue => ANSI_BRIGHT[4],
        NamedColor::BrightMagenta => ANSI_BRIGHT[5],
        NamedColor::BrightCyan => ANSI_BRIGHT[6],
        NamedColor::BrightWhite => ANSI_BRIGHT[7],
        NamedColor::DimBlack => ANSI_NORMAL[0],
        NamedColor::DimRed => ANSI_NORMAL[1],
        NamedColor::DimGreen => ANSI_NORMAL[2],
        NamedColor::DimYellow => ANSI_NORMAL[3],
        NamedColor::DimBlue => ANSI_NORMAL[4],
        NamedColor::DimMagenta => ANSI_NORMAL[5],
        NamedColor::DimCyan => ANSI_NORMAL[6],
        NamedColor::DimWhite => ANSI_NORMAL[7],
        // Cursor color and dim/bright foreground/background variants fall
        // back to the defaults.
        _ => TERM_FG,
    };
    let mut hsla = hex(value);
    if dim {
        hsla.l *= 0.66;
    }
    hsla
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
        div()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .bg(theme::panel())
            .border_r_1()
            .border_color(theme::border())
            .flex()
            .flex_col()
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
                    |this, _window, cx| {
                        this.connect_selected(cx);
                    },
                )),
            )
            .child(section_label("RECENT SESSIONS"))
            .child(self.render_recents(cx))
            .child(section_label("FILES"))
            .child(self.render_active_files(cx))
            .into_any_element()
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
            render_tree_rows(tree, 0, tree_selection.as_ref(), cx, &mut rows);
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
            // targets that directory instead (handled per row).
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
    cx: &mut Context<RootView>,
    rows: &mut Vec<gpui::AnyElement>,
) {
    for node in nodes {
        let path = node.entry.path.clone();
        let is_selected = selected == Some(&node.entry.path);
        let icon = if node.entry.is_dir {
            if node.loading {
                "▸ …"
            } else if node.expanded {
                "▾"
            } else {
                "▸"
            }
        } else {
            "•"
        };
        rows.push(
            div()
                .id(SharedString::from(format!("tree:{}", node.entry.path.display())))
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .px_2()
                .py(px(1.))
                .pl(px(8. + depth as f32 * 14.))
                .cursor_pointer()
                .rounded_sm()
                .when(is_selected, |row| row.bg(theme::selection()))
                .hover(|row| row.bg(theme::hover()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_tree_node(path.clone(), cx);
                }))
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
                // Directory rows accept OS file drops and upload into that
                // directory (recursively).
                .when(node.entry.is_dir, |row| {
                    let target = node.entry.path.clone();
                    row.can_drop(|value, _, _| value.is::<ExternalPaths>())
                        .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                            let paths = paths.paths().to_vec();
                            this.upload_dropped_paths(&paths, target.clone(), cx);
                        }))
                        .drag_over::<ExternalPaths>(|style, _, _, _| {
                            style.bg(theme::selection())
                        })
                })
                .child(
                    div()
                        .w(px(16.))
                        .text_color(theme::text_dim())
                        .child(icon.to_string()),
                )
                .child(
                    div()
                        .truncate()
                        .when(!node.entry.is_dir, |name| name.text_color(theme::text_dim()))
                        .child(node.entry.name.clone()),
                )
                .when(!node.entry.is_dir, |row| {
                    row.child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(theme::text_dim())
                            .child(format_size(node.entry.size)),
                    )
                })
                .into_any_element(),
        );
        if node.expanded {
            if let Some(children) = node.children.as_ref() {
                render_tree_rows(children, depth + 1, selected, cx, rows);
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
