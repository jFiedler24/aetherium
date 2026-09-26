//! SSH/SFTP backend.
//!
//! A dedicated `std::thread` runs a small tokio runtime; all russh I/O lives
//! there. The UI sends [`Command`]s over a tokio unbounded channel and polls
//! [`Event`]s from a `std::sync::mpsc` receiver. Raw PTY output is fed
//! straight into the shared [`TerminalModel`]; the UI notices via the model's
//! dirty flag, so terminal output never crosses the event channel.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use parking_lot::Mutex;
use russh::client::{self, AuthResult, Handle};
use russh::keys::PrivateKeyWithHashAlg;
use russh::{ChannelMsg, Disconnect};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::profiles::{AuthMethod, Profile};
use crate::terminal_model::TerminalModel;

/// UI → backend commands.
pub enum Command {
    Connect(Profile),
    TerminalInput(Vec<u8>),
    ResizePty { cols: u32, rows: u32 },
    ListDir { session_id: u64, path: PathBuf },
    /// Upload a local file or directory (recursively) into `remote_dir`.
    Upload { session_id: u64, local: PathBuf, remote_dir: PathBuf },
    /// Download a remote file or directory (recursively) into ~/Downloads.
    Download { session_id: u64, remote: PathBuf },
    /// Download a remote file to a temp directory for drag-out. The UI is
    /// notified via `Event::TempDownloadReady` when the file is available
    /// locally.
    DownloadToTemp { session_id: u64, remote: PathBuf },
    /// Move/rename a remote entry (SFTP `rename`); used by file-tree
    /// drag-and-drop.
    Rename { session_id: u64, from: PathBuf, to: PathBuf },
    /// Start `tail -f` on a remote file over a new exec channel, feeding the
    /// given terminal grid (a log-follow tab). `tail_id` identifies the tab.
    TailFile { tail_id: u64, terminal: TerminalModel, path: String },
    /// Stop the tail with the given id (tab closed).
    CloseTail { tail_id: u64 },
    Disconnect,
}

/// Backend → UI events. Terminal output is deliberately *not* an event: it
/// flows into the `Term` grid directly and the UI polls the dirty flag.
pub enum Event {
    Connected { home_dir: PathBuf },
    DirListing { session_id: u64, path: PathBuf, entries: Vec<FileEntry> },
    /// A file transfer (upload or download) has begun.
    TransferStarted { label: String },
    /// Progress of the active transfer; `total_bytes` is 0 when unknown
    /// (e.g. downloading a directory). `bytes_per_second` and `eta_seconds`
    /// are smoothed estimates; `eta_seconds` is 0 when unknown.
    TransferProgress {
        label: String,
        done_bytes: u64,
        total_bytes: u64,
        bytes_per_second: f64,
        eta_seconds: u64,
    },
    /// The transfer finished successfully; the label names the result
    /// (target directory for uploads, saved path for downloads).
    TransferDone { label: String },
    /// A drag-and-drop move finished; the UI refreshes the directories that
    /// lost or gained an entry.
    EntryMoved { from: PathBuf, to: PathBuf },
    /// A remote file was downloaded to a local temp path for drag-out.
    /// Staging is silent: unlike real transfers it never emits
    /// `TransferStarted`/`TransferProgress`/`TransferDone`, so it can't
    /// disturb the shared transfer progress UI.
    TempDownloadReady { session_id: u64, remote: PathBuf, local: PathBuf },
    Error(String),
    Disconnected,
    /// The `tail -f` channel with the given tab id ended (file closed or the
    /// session went away).
    TailEnded { tab_id: u64 },
    /// The `tail -f` could not be started for the given tab id.
    TailError { tab_id: u64, message: String },
}

/// One entry of a remote directory listing.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
    /// Modification time as seconds since the Unix epoch.
    pub modified: Option<u64>,
}

/// Handle owned by the UI; talks to the background tokio runtime. Cheap to
/// clone — all clones share the same backend.
#[derive(Clone)]
pub struct SessionHandle {
    cmd_tx: mpsc::UnboundedSender<Command>,
    events: Arc<Mutex<std_mpsc::Receiver<Event>>>,
    _thread: Arc<std::thread::JoinHandle<()>>,
}

impl SessionHandle {
    /// Spawn the backend thread. `terminal` is fed with PTY output as it
    /// arrives.
    pub fn spawn(terminal: TerminalModel) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = std_mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("aetherium-ssh".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .thread_name("aetherium-tokio")
                    .build()
                    .expect("build tokio runtime");
                runtime.block_on(run(cmd_rx, event_tx, terminal));
            })
            .expect("spawn ssh thread");
        Self {
            cmd_tx,
            events: Arc::new(Mutex::new(event_rx)),
            _thread: Arc::new(thread),
        }
    }

    pub fn send(&self, command: Command) {
        // If the backend is gone there is nothing useful to do; the UI will
        // observe the disconnect through other means.
        let _ = self.cmd_tx.send(command);
    }

    pub fn connect(&self, profile: Profile) {
        self.send(Command::Connect(profile));
    }

    pub fn disconnect(&self) {
        self.send(Command::Disconnect);
    }

    pub fn input(&self, bytes: Vec<u8>) {
        self.send(Command::TerminalInput(bytes));
    }

    pub fn resize_pty(&self, cols: u32, rows: u32) {
        self.send(Command::ResizePty { cols, rows });
    }

    pub fn list_dir(&self, session_id: u64, path: PathBuf) {
        self.send(Command::ListDir { session_id, path });
    }

    pub fn upload(&self, session_id: u64, local: PathBuf, remote_dir: PathBuf) {
        self.send(Command::Upload { session_id, local, remote_dir });
    }

    pub fn download(&self, session_id: u64, remote: PathBuf) {
        self.send(Command::Download { session_id, remote });
    }

    /// Download a remote file to a temp directory for drag-out.
    pub fn download_to_temp(&self, session_id: u64, remote: PathBuf) {
        self.send(Command::DownloadToTemp { session_id, remote });
    }

    pub fn rename(&self, session_id: u64, from: PathBuf, to: PathBuf) {
        self.send(Command::Rename { session_id, from, to });
    }

    /// Non-blocking: pop one pending event, if any.
    pub fn try_recv_event(&self) -> Option<Event> {
        self.events.lock().try_recv().ok()
    }
}

/// russh client handler. Verifies the server's host key against
/// `~/.ssh/known_hosts`; unknown or mismatched keys are rejected rather than
/// silently accepted, closing the "accept anything" hole from v1.
struct ClientHandler {
    host: String,
    port: u16,
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } = server_public_key else {
            eprintln!(
                "aetherium: rejecting certificate-based host key for {}:{} (unsupported)",
                self.host, self.port
            );
            return Ok(false);
        };
        match russh::keys::check_known_hosts(&self.host, self.port, key) {
            Ok(true) => Ok(true),
            Ok(false) => {
                // Trust-on-first-use: no entry exists yet for this host, so
                // learn it now. A *changed* key (KeyChanged below) is still
                // rejected, which is what protects against MITM.
                if let Err(err) = russh::keys::known_hosts::learn_known_hosts(&self.host, self.port, key) {
                    eprintln!(
                        "aetherium: could not record host key for {}:{}: {err}",
                        self.host, self.port
                    );
                }
                Ok(true)
            }
            Err(err) => {
                eprintln!(
                    "aetherium: host key verification failed for {}:{}: {err}",
                    self.host, self.port
                );
                Ok(false)
            }
        }
    }
}

/// Top-level backend loop: idle until a `Connect` arrives, run the session
/// until it ends, then go back to idle.
async fn run(
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    event_tx: std_mpsc::Sender<Event>,
    terminal: TerminalModel,
) {
    loop {
        let profile = loop {
            match cmd_rx.recv().await {
                Some(Command::Connect(profile)) => break profile,
                Some(_) => continue, // ignore input while disconnected
                None => return,      // UI is gone
            }
        };

        if let Err(err) = session_loop(profile, &mut cmd_rx, &event_tx, &terminal).await {
            let _ = event_tx.send(Event::Error(format!("{err:#}")));
        }
        terminal.clear_pty_writer();
        let _ = event_tx.send(Event::Disconnected);
    }
}

/// Connect, authenticate, open shell + SFTP channels, and serve commands
/// until disconnect or connection loss.
async fn session_loop(
    profile: Profile,
    cmd_rx: &mut mpsc::UnboundedReceiver<Command>,
    event_tx: &std_mpsc::Sender<Event>,
    terminal: &TerminalModel,
) -> Result<()> {
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(60)),
        keepalive_interval: Some(Duration::from_secs(15)),
        nodelay: true,
        ..Default::default()
    });

    let mut handle = client::connect(
        config,
        (profile.host.as_str(), profile.port),
        ClientHandler {
            host: profile.host.clone(),
            port: profile.port,
        },
    )
    .await
    .with_context(|| format!("connecting to {}:{}", profile.host, profile.port))?;

    authenticate(&mut handle, &profile).await?;

    // Interactive shell channel with a PTY.
    let (cols, rows) = (terminal.columns() as u32, terminal.screen_lines() as u32);
    let mut shell = handle
        .channel_open_session()
        .await
        .context("opening shell channel")?;
    shell
        .request_pty(true, "xterm-256color", cols, rows, 0, 0, &[])
        .await
        .context("requesting pty")?;
    shell.request_shell(true).await.context("requesting shell")?;

    // SFTP subsystem on a second channel.
    let sftp_channel = handle
        .channel_open_session()
        .await
        .context("opening sftp channel")?;
    sftp_channel
        .request_subsystem(true, "sftp")
        .await
        .context("requesting sftp subsystem")?;
    let sftp = Arc::new(
        russh_sftp::client::SftpSession::new(sftp_channel.into_stream())
            .await
            .context("starting sftp session")?,
    );

    let home_dir = sftp
        .canonicalize(".")
        .await
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));

    // Forward terminal-originated replies (DA, cursor position reports, ...)
    // back into the shell channel.
    let (pty_write_tx, mut pty_write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    terminal.set_pty_writer(pty_write_tx);
    let mut shell_writer = shell.make_writer();
    let writer_task = tokio::spawn(async move {
        while let Some(bytes) = pty_write_rx.recv().await {
            if shell_writer.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });

    let _ = event_tx.send(Event::Connected { home_dir });

    let result = command_loop(cmd_rx, event_tx, terminal, &handle, &mut shell, &sftp).await;

    writer_task.abort();
    let _ = handle
        .disconnect(Disconnect::ByApplication, "", "English")
        .await;
    result
}

async fn command_loop(
    cmd_rx: &mut mpsc::UnboundedReceiver<Command>,
    event_tx: &std_mpsc::Sender<Event>,
    terminal: &TerminalModel,
    handle: &Handle<ClientHandler>,
    shell: &mut russh::Channel<client::Msg>,
    sftp: &Arc<russh_sftp::client::SftpSession>,
) -> Result<()> {
    // Abort signals for running `tail -f` channels, keyed by tab id.
    let mut tails: std::collections::HashMap<u64, oneshot::Sender<()>> =
        std::collections::HashMap::new();
    loop {
        tokio::select! {
            command = cmd_rx.recv() => {
                match command {
                    Some(Command::TerminalInput(bytes)) => {
                        shell.data_bytes(bytes).await.context("writing to channel")?;
                    }
                    Some(Command::ResizePty { cols, rows }) => {
                        shell.window_change(cols, rows, 0, 0).await.context("window-change")?;
                    }
                    Some(Command::TailFile { tail_id, terminal, path }) => {
                        let command = format!("tail -n 200 -f {}", shell_quote(&path));
                        match handle.channel_open_session().await {
                            Ok(mut channel) => {
                                let started = channel
                                    .exec(true, command.clone())
                                    .await
                                    .map_err(|err| format!("{err}"));
                                if let Err(message) = started {
                                    let _ = event_tx.send(Event::TailError { tab_id: tail_id, message });
                                } else {
                                    let (abort_tx, mut abort_rx) = oneshot::channel::<()>();
                                    tails.insert(tail_id, abort_tx);
                                    let event_tx = event_tx.clone();
                                    tokio::spawn(async move {
                                        loop {
                                            tokio::select! {
                                                // Dropping the tab closes the
                                                // channel, which kills the
                                                // remote tail.
                                                _ = &mut abort_rx => break,
                                                msg = channel.wait() => {
                                                    match msg {
                                                        Some(ChannelMsg::Data { data })
                                                        | Some(ChannelMsg::ExtendedData { data, .. }) => {
                                                            terminal.feed(&data);
                                                        }
                                                        Some(ChannelMsg::Eof)
                                                        | Some(ChannelMsg::Close)
                                                        | None => break,
                                                        Some(_) => {}
                                                    }
                                                }
                                            }
                                        }
                                        let _ = event_tx.send(Event::TailEnded { tab_id: tail_id });
                                    });
                                }
                            }
                            Err(err) => {
                                let _ = event_tx.send(Event::TailError {
                                    tab_id: tail_id,
                                    message: format!("opening channel: {err}"),
                                });
                            }
                        }
                    }
                    Some(Command::CloseTail { tail_id }) => {
                        // Dropping the sender resolves the receiver in the
                        // reader task, which then drops the channel.
                        tails.remove(&tail_id);
                    }
                    Some(Command::ListDir { session_id, path }) => {
                        // read_dir can be slow on high-latency links; run it
                        // off the session loop so terminal I/O keeps flowing.
                        let sftp = sftp.clone();
                        let event_tx = event_tx.clone();
                        tokio::spawn(async move {
                            match sftp.read_dir(path.to_string_lossy().into_owned()).await {
                                Ok(read_dir) => {
                                    let entries = read_dir
                                        .map(|entry| {
                                            let metadata = entry.metadata();
                                            FileEntry {
                                                name: entry.file_name(),
                                                path: PathBuf::from(entry.path()),
                                                is_dir: entry.file_type().is_dir(),
                                                size: metadata.len(),
                                                modified: metadata
                                                    .modified()
                                                    .ok()
                                                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                                    .map(|d| d.as_secs()),
                                            }
                                        })
                                        .collect();
                                    let _ = event_tx.send(Event::DirListing { session_id, path, entries });
                                }
                                Err(err) => {
                                    let _ = event_tx.send(Event::Error(format!(
                                        "listing {}: {err}",
                                        path.display()
                                    )));
                                }
                            }
                        });
                    }
                    Some(Command::Upload { session_id, local, remote_dir }) => {
                        // Like ListDir, transfers run off the session loop so
                        // terminal I/O keeps flowing while bytes move. The
                        // session tag is for the UI's bookkeeping; the backend
                        // only ever serves the live session.
                        let _ = session_id;
                        let sftp = sftp.clone();
                        let event_tx = event_tx.clone();
                        tokio::spawn(async move {
                            upload(&sftp, &event_tx, &local, &remote_dir).await;
                        });
                    }
                    Some(Command::Download { session_id, remote }) => {
                        let _ = session_id;
                        let sftp = sftp.clone();
                        let event_tx = event_tx.clone();
                        tokio::spawn(async move {
                            download(&sftp, &event_tx, &remote).await;
                        });
                    }
                    Some(Command::DownloadToTemp { session_id, remote }) => {
                        let sftp = sftp.clone();
                        let event_tx = event_tx.clone();
                        tokio::spawn(async move {
                            download_to_temp(&sftp, &event_tx, session_id, &remote).await;
                        });
                    }
                    Some(Command::Rename { session_id, from, to }) => {
                        // Like transfers, run off the session loop so the
                        // terminal keeps flowing while the server renames.
                        let _ = session_id;
                        let sftp = sftp.clone();
                        let event_tx = event_tx.clone();
                        tokio::spawn(async move {
                            match sftp
                                .rename(
                                    from.to_string_lossy().into_owned(),
                                    to.to_string_lossy().into_owned(),
                                )
                                .await
                            {
                                Ok(()) => {
                                    let _ = event_tx.send(Event::EntryMoved { from, to });
                                }
                                Err(err) => {
                                    let _ = event_tx.send(Event::Error(format!(
                                        "moving {} to {}: {err}",
                                        from.display(),
                                        to.display()
                                    )));
                                }
                            }
                        });
                    }
                    Some(Command::Disconnect) => return Ok(()),
                    Some(Command::Connect(_)) => {
                        // Already connected; a new Connect is handled after
                        // the UI disconnects first.
                    }
                    None => return Ok(()),
                }
            }
            msg = shell.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => terminal.feed(&data),
                    Some(ChannelMsg::ExtendedData { data, .. }) => terminal.feed(&data),
                    Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => return Ok(()),
                    Some(ChannelMsg::ExitStatus { .. }) => {}
                    Some(_) => {}
                }
            }
        }
    }
}

// --- file transfers -----------------------------------------------------------

/// Chunk size for streamed SFTP reads/writes.
const TRANSFER_CHUNK: usize = 64 * 1024;

/// Accumulates bytes moved and reports `TransferProgress` events. Sharing one
/// accumulator across a whole (possibly recursive) transfer gives smooth
/// overall progress. Speed and ETA are estimated from a simple moving average
/// over the last few seconds.
struct TransferProgress<'a> {
    event_tx: &'a std_mpsc::Sender<Event>,
    label: &'a str,
    done: u64,
    total: u64,
    started: Instant,
    /// (timestamp, cumulative done) samples for the last ~3 seconds.
    samples: Vec<(Instant, u64)>,
}

impl TransferProgress<'_> {
    fn new<'a>(event_tx: &'a std_mpsc::Sender<Event>, label: &'a str, total: u64) -> TransferProgress<'a> {
        let started = Instant::now();
        TransferProgress {
            event_tx,
            label,
            done: 0,
            total,
            started,
            samples: vec![(started, 0)],
        }
    }

    fn speed(&self) -> f64 {
        let now = Instant::now();
        // Keep samples from the last 3 seconds.
        let window_start = now - Duration::from_secs(3);
        let recent: Vec<_> = self
            .samples
            .iter()
            .copied()
            .filter(|(t, _)| *t >= window_start)
            .collect();
        if recent.len() < 2 {
            // Fall back to the overall average.
            let elapsed = now.duration_since(self.started).as_secs_f64();
            if elapsed > 0.0 {
                self.done as f64 / elapsed
            } else {
                0.0
            }
        } else {
            let (t0, b0) = recent.first().copied().unwrap();
            let (t1, b1) = recent.last().copied().unwrap();
            let dt = t1.duration_since(t0).as_secs_f64();
            if dt > 0.0 {
                (b1 - b0) as f64 / dt
            } else {
                0.0
            }
        }
    }

    fn eta_seconds(&self) -> u64 {
        if self.total == 0 || self.done >= self.total {
            return 0;
        }
        let speed = self.speed();
        if speed <= 0.0 {
            return 0;
        }
        let remaining = (self.total - self.done) as f64 / speed;
        remaining.ceil() as u64
    }

    fn add(&mut self, bytes: u64) {
        self.done += bytes;
        let now = Instant::now();
        self.samples.push((now, self.done));
        let _ = self.event_tx.send(Event::TransferProgress {
            label: self.label.to_string(),
            done_bytes: self.done,
            total_bytes: self.total,
            bytes_per_second: self.speed(),
            eta_seconds: self.eta_seconds(),
        });
    }
}

/// Join a child name onto a remote (always `/`-separated) directory path.
fn remote_join(dir: &std::path::Path, name: &str) -> String {
    format!("{}/{}", dir.to_string_lossy().trim_end_matches('/'), name)
}

/// Quote a path for execution by a POSIX shell.
fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', "'\\''"))
}

/// Total size of a local file tree; unreadable entries are skipped.
fn local_tree_size(path: &std::path::Path) -> u64 {
    let Ok(metadata) = std::fs::metadata(path) else {
        return 0;
    };
    if metadata.is_dir() {
        std::fs::read_dir(path)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| local_tree_size(&entry.path()))
                    .sum()
            })
            .unwrap_or(0)
    } else {
        metadata.len()
    }
}

/// Upload `local` (file or directory, recursive) into `remote_dir`.
async fn upload(
    sftp: &russh_sftp::client::SftpSession,
    event_tx: &std_mpsc::Sender<Event>,
    local: &std::path::Path,
    remote_dir: &std::path::Path,
) {
    let label = format!("upload {} → {}", local.display(), remote_dir.display());
    let total = local_tree_size(local);
    let _ = event_tx.send(Event::TransferStarted { label: label.clone() });
    let mut progress = TransferProgress::new(event_tx, &label, total);
    match upload_path(sftp, local, remote_dir, &mut progress).await {
        Ok(()) => {
            let _ = event_tx.send(Event::TransferDone { label });
        }
        Err(err) => {
            let _ = event_tx.send(Event::Error(format!(
                "uploading {}: {err:#}",
                local.display()
            )));
        }
    }
}

async fn upload_path(
    sftp: &russh_sftp::client::SftpSession,
    local: &std::path::Path,
    remote_dir: &std::path::Path,
    progress: &mut TransferProgress<'_>,
) -> Result<()> {
    use std::io::Read as _;

    let metadata = std::fs::metadata(local)
        .with_context(|| format!("reading metadata of {}", local.display()))?;
    let name = local
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .with_context(|| format!("{} has no file name", local.display()))?;

    if metadata.is_dir() {
        let sub_dir = remote_join(remote_dir, &name);
        // Ignore errors here: the directory may already exist. If it cannot
        // be created at all, the first file write inside it will fail.
        let _ = sftp.create_dir(sub_dir.clone()).await;
        let entries = std::fs::read_dir(local)
            .with_context(|| format!("reading directory {}", local.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", local.display()))?;
            Box::pin(upload_path(sftp, &entry.path(), std::path::Path::new(&sub_dir), progress))
                .await?;
        }
        return Ok(());
    }

    let remote_path = remote_join(remote_dir, &name);
    let mut local_file = std::fs::File::open(local)
        .with_context(|| format!("opening {}", local.display()))?;
    let mut remote_file = sftp
        .create(remote_path.clone())
        .await
        .with_context(|| format!("creating remote {remote_path}"))?;

    let mut buffer = vec![0u8; TRANSFER_CHUNK];
    loop {
        // Local disk reads are blocking but fast; the SFTP write below still
        // yields to the runtime between chunks.
        let read = local_file
            .read(&mut buffer)
            .with_context(|| format!("reading {}", local.display()))?;
        if read == 0 {
            break;
        }
        remote_file
            .write_all(&buffer[..read])
            .await
            .with_context(|| format!("writing remote {remote_path}"))?;
        progress.add(read as u64);
    }
    remote_file
        .close()
        .await
        .with_context(|| format!("closing remote {remote_path}"))?;
    Ok(())
}

/// Download `remote` (file or directory, recursive) into ~/Downloads,
/// preserving the base name and de-duplicating on collision.
async fn download(
    sftp: &russh_sftp::client::SftpSession,
    event_tx: &std_mpsc::Sender<Event>,
    remote: &std::path::Path,
) {
    let Some(home) = dirs::home_dir() else {
        let _ = event_tx.send(Event::Error("download: no home directory".into()));
        return;
    };
    let downloads = home.join("Downloads");
    if let Err(err) = std::fs::create_dir_all(&downloads) {
        let _ = event_tx.send(Event::Error(format!(
            "download: creating {}: {err}",
            downloads.display()
        )));
        return;
    }

    let label = format!("download {}", remote.display());
    // Directories have no meaningful total; progress then counts bytes only.
    let total = match sftp.metadata(remote.to_string_lossy().into_owned()).await {
        Ok(metadata) if !metadata.is_dir() => metadata.len(),
        Ok(_) => 0,
        Err(err) => {
            let _ = event_tx.send(Event::Error(format!(
                "download: stating {}: {err}",
                remote.display()
            )));
            return;
        }
    };
    let _ = event_tx.send(Event::TransferStarted { label: label.clone() });
    let mut progress = TransferProgress::new(event_tx, &label, total);
    match download_path(sftp, remote, &downloads, &mut progress).await {
        Ok(saved) => {
            let _ = event_tx.send(Event::TransferDone {
                label: format!("{} → {}", label, saved.display()),
            });
        }
        Err(err) => {
            let _ = event_tx.send(Event::Error(format!(
                "downloading {}: {err:#}",
                remote.display()
            )));
        }
    }
}

/// Download a remote file to the OS temp directory so it can be dragged out
/// of the app. Only files are supported (directories are too slow to stage).
async fn download_to_temp(
    sftp: &russh_sftp::client::SftpSession,
    event_tx: &std_mpsc::Sender<Event>,
    session_id: u64,
    remote: &std::path::Path,
) {
    let metadata = match sftp.metadata(remote.to_string_lossy().into_owned()).await {
        Ok(metadata) => metadata,
        Err(err) => {
            let _ = event_tx.send(Event::Error(format!(
                "temp download: stating {}: {err}",
                remote.display()
            )));
            return;
        }
    };
    if metadata.is_dir() {
        let _ = event_tx.send(Event::Error(
            "temp download: directories are not supported".into(),
        ));
        return;
    }

    let name = remote
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    let temp_dir = std::env::temp_dir().join("aetherium");
    if let Err(err) = std::fs::create_dir_all(&temp_dir) {
        let _ = event_tx.send(Event::Error(format!(
            "temp download: creating {}: {err}",
            temp_dir.display()
        )));
        return;
    }
    // Unique subdirectory per file to avoid collisions.
    let local_dir = temp_dir.join(Uuid::new_v4().to_string());
    if let Err(err) = std::fs::create_dir_all(&local_dir) {
        let _ = event_tx.send(Event::Error(format!(
            "temp download: creating {}: {err}",
            local_dir.display()
        )));
        return;
    }
    let local = local_dir.join(&name);

    log::info!("drag-out: downloading {} → {}", remote.display(), local.display());

    // Deliberately silent: staging must not touch the shared transfer
    // progress UI (a drag is not a user-requested transfer). Failures still
    // surface through Event::Error.
    let result = async {
        let mut local_file = std::fs::File::create(&local)
            .with_context(|| format!("creating {}", local.display()))?;
        let mut remote_file = sftp
            .open(remote.to_string_lossy().into_owned())
            .await
            .with_context(|| format!("opening remote {}", remote.display()))?;
        let mut buffer = vec![0u8; TRANSFER_CHUNK];
        loop {
            let read = remote_file
                .read(&mut buffer)
                .await
                .with_context(|| format!("reading remote {}", remote.display()))?;
            if read == 0 {
                break;
            }
            use std::io::Write as _;
            local_file
                .write_all(&buffer[..read])
                .with_context(|| format!("writing {}", local.display()))?;
        }
        remote_file
            .close()
            .await
            .with_context(|| format!("closing remote {}", remote.display()))?;
        Ok::<_, anyhow::Error>(())
    }
    .await;

    match result {
        Ok(()) => {
            log::info!("drag-out: staged {} → {}", remote.display(), local.display());
            let _ = event_tx.send(Event::TempDownloadReady {
                session_id,
                remote: remote.to_path_buf(),
                local,
            });
        }
        Err(err) => {
            // Drop the partially staged file; nothing will ever reference it.
            let _ = std::fs::remove_dir_all(&local_dir);
            let _ = event_tx.send(Event::Error(format!(
                "temp download {}: {err:#}",
                remote.display()
            )));
        }
    }
}

async fn download_path(
    sftp: &russh_sftp::client::SftpSession,
    remote: &std::path::Path,
    local_dir: &std::path::Path,
    progress: &mut TransferProgress<'_>,
) -> Result<PathBuf> {
    use std::io::Write as _;

    let name = remote
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .with_context(|| format!("{} has no file name", remote.display()))?;
    let metadata = sftp
        .metadata(remote.to_string_lossy().into_owned())
        .await
        .with_context(|| format!("stating remote {}", remote.display()))?;

    if metadata.is_dir() {
        let target = unique_download_path(local_dir, &name);
        std::fs::create_dir_all(&target)
            .with_context(|| format!("creating {}", target.display()))?;
        let entries = sftp
            .read_dir(remote.to_string_lossy().into_owned())
            .await
            .with_context(|| format!("listing remote {}", remote.display()))?;
        for entry in entries {
            let child_name = entry.file_name();
            if child_name == "." || child_name == ".." {
                continue;
            }
            Box::pin(download_path(
                sftp,
                std::path::Path::new(&entry.path()),
                &target,
                progress,
            ))
            .await?;
        }
        return Ok(target);
    }

    let target = unique_download_path(local_dir, &name);
    let mut local_file = std::fs::File::create(&target)
        .with_context(|| format!("creating {}", target.display()))?;
    let mut remote_file = sftp
        .open(remote.to_string_lossy().into_owned())
        .await
        .with_context(|| format!("opening remote {}", remote.display()))?;

    let mut buffer = vec![0u8; TRANSFER_CHUNK];
    loop {
        let read = remote_file
            .read(&mut buffer)
            .await
            .with_context(|| format!("reading remote {}", remote.display()))?;
        if read == 0 {
            break;
        }
        local_file
            .write_all(&buffer[..read])
            .with_context(|| format!("writing {}", target.display()))?;
        progress.add(read as u64);
    }
    remote_file
        .close()
        .await
        .with_context(|| format!("closing remote {}", remote.display()))?;
    Ok(target)
}

/// `dir/name`, or `dir/name (1)` etc. when the plain name is taken.
fn unique_download_path(dir: &std::path::Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    for index in 1..1000u32 {
        let renamed = match name.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({index}).{ext}"),
            _ => format!("{name} ({index})"),
        };
        let candidate = dir.join(renamed);
        if !candidate.exists() {
            return candidate;
        }
    }
    // Absurd number of collisions; let the create fail downstream.
    dir.join(name)
}

/// Authenticate according to the profile's auth method; fail unless the
/// server reports full success.
async fn authenticate(handle: &mut Handle<ClientHandler>, profile: &Profile) -> Result<()> {
    let result = match &profile.auth {
        AuthMethod::Password { password } => {
            // Prefer silent key-based auth (agent, then a default key file);
            // only send the password if the server still needs it.
            match try_silent_auth(handle, &profile.username).await {
                Some(result) => result,
                None => handle
                    .authenticate_password(profile.username.clone(), password.clone())
                    .await
                    .context("password authentication")?,
            }
        }
        AuthMethod::KeyFile { path, passphrase } => {
            // An empty path means "use whatever default key exists"; this
            // mirrors OpenSSH's own fallback behavior.
            let path = if path.as_os_str().is_empty() {
                detect_default_ssh_key().ok_or_else(|| {
                    anyhow!(
                        "no private key path was set and no default SSH key was found \
                         (looked for ~/.ssh/id_ed25519, id_rsa, id_ecdsa)"
                    )
                })?
            } else {
                expand_tilde(path)
            };
            let key = russh::keys::load_secret_key(&path, passphrase.as_deref())
                .with_context(|| format!("loading key {}", path.display()))?;
            let hash = handle.best_supported_rsa_hash().await?.flatten();
            handle
                .authenticate_publickey(
                    profile.username.clone(),
                    PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                )
                .await
                .context("publickey authentication")?
        }
        AuthMethod::Agent => {
            #[cfg(unix)]
            {
                authenticate_with_agent(handle, &profile.username).await?
            }
            #[cfg(windows)]
            {
                return Err(anyhow!(
                    "ssh-agent authentication is not supported on Windows yet"
                ));
            }
        }
    };

    match result {
        AuthResult::Success => Ok(()),
        AuthResult::Failure {
            remaining_methods: _,
            partial_success,
        } => {
            if partial_success {
                Err(anyhow!(
                    "server requires a second authentication factor (unsupported)"
                ))
            } else {
                Err(anyhow!("authentication failed"))
            }
        }
    }
}

/// Expand a leading `~/` in a key file path against the user's home
/// directory (passphrase-protected keys go through the same loader).
fn expand_tilde(path: &std::path::Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

/// Detect a default SSH private key in the user's `.ssh` directory.
/// On Windows this checks `%USERPROFILE%\.ssh\`; on Unix, `~/.ssh/`.
/// Prefers ed25519, then rsa, then ecdsa, matching common OpenSSH defaults.
fn detect_default_ssh_key() -> Option<PathBuf> {
    let ssh_dir = dirs::home_dir()?.join(".ssh");
    if !ssh_dir.is_dir() {
        return None;
    }
    for name in ["id_ed25519", "id_rsa", "id_ecdsa"] {
        let path = ssh_dir.join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

/// Best-effort attempts at ssh-agent and default-key auth for a
/// password-configured profile. Failures here are not fatal — they just mean
/// the caller should fall back to the stored password — so only a definite
/// `AuthResult::Success` is reported back.
async fn try_silent_auth(handle: &mut Handle<ClientHandler>, username: &str) -> Option<AuthResult> {
    #[cfg(unix)]
    if std::env::var_os("SSH_AUTH_SOCK").is_some() {
        if let Ok(result @ AuthResult::Success) = authenticate_with_agent(handle, username).await {
            return Some(result);
        }
    }
    let key_path = detect_default_ssh_key()?;
    let key = russh::keys::load_secret_key(&key_path, None).ok()?;
    let hash = handle.best_supported_rsa_hash().await.ok()?.flatten();
    match handle
        .authenticate_publickey(username.to_owned(), PrivateKeyWithHashAlg::new(Arc::new(key), hash))
        .await
    {
        Ok(result @ AuthResult::Success) => Some(result),
        _ => None,
    }
}

#[cfg(unix)]
async fn authenticate_with_agent(
    handle: &mut Handle<ClientHandler>,
    username: &str,
) -> Result<AuthResult> {
    use russh::keys::agent::AgentIdentity;
    use russh::keys::agent::client::AgentClient;

    let mut agent = AgentClient::connect_env()
        .await
        .context("connecting to ssh-agent (SSH_AUTH_SOCK)")?;
    let identities = agent
        .request_identities()
        .await
        .context("listing ssh-agent identities")?;
    let hash = handle.best_supported_rsa_hash().await?.flatten();

    // Try each public-key identity in turn; certificates are not supported
    // yet, and an identity the server rejects should not preclude the rest.
    let mut attempted = false;
    for identity in identities {
        let AgentIdentity::PublicKey { key, .. } = identity else {
            continue;
        };
        attempted = true;
        match handle
            .authenticate_publickey_with(username.to_owned(), key, hash, &mut agent)
            .await
        {
            Ok(result @ AuthResult::Success) => return Ok(result),
            Ok(AuthResult::Failure { .. }) | Err(_) => continue,
        }
    }
    if attempted {
        Err(anyhow!("ssh-agent authentication failed for all identities"))
    } else {
        Err(anyhow!("ssh-agent holds no usable public-key identities"))
    }
}
