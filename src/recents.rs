//! Recently connected sessions and TOML persistence.
//!
//! Recents are stored at `~/.config/aetherium/recents.toml`, deduplicated by
//! `(host, username, port)`, most-recent-first, capped at 10 entries.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// Maximum number of remembered sessions.
const MAX_RECENT: usize = 10;

/// One remembered session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentEntry {
    pub profile_name: String,
    pub host: String,
    pub username: String,
    pub port: u16,
    /// Connection time as seconds since the Unix epoch.
    pub connected_at_unix: u64,
}

/// On-disk representation of the recents file.
#[derive(Debug, Default, Serialize, Deserialize)]
struct RecentFile {
    #[serde(default)]
    recent: Vec<RecentEntry>,
}

/// The collection of recent sessions plus the path they persist to.
pub struct RecentStore {
    pub entries: Vec<RecentEntry>,
    path: PathBuf,
}

impl RecentStore {
    /// Default location: `~/.config/aetherium/recents.toml`.
    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("aetherium")
            .join("recents.toml")
    }

    /// Load from the default location, creating an empty store (and parent
    /// directory) if the file is missing.
    pub fn load() -> Self {
        Self::load_from(Self::default_path())
    }

    /// Load from an explicit path. Missing files yield an empty store; a
    /// corrupt file is backed up to `.bak` before being ignored.
    pub fn load_from(path: PathBuf) -> Self {
        let entries = match fs::read_to_string(&path) {
            Ok(text) => toml::from_str::<RecentFile>(&text)
                .map(|file| file.recent)
                .unwrap_or_else(|err| {
                    // Keep the corrupt file around for manual recovery.
                    let backup = PathBuf::from(format!("{}.bak", path.display()));
                    eprintln!(
                        "aetherium: failed to parse {}: {err}; backing up to {}",
                        path.display(),
                        backup.display()
                    );
                    if let Err(copy_err) = fs::copy(&path, &backup) {
                        eprintln!(
                            "aetherium: failed to back up {}: {copy_err}",
                            path.display()
                        );
                    }
                    Vec::new()
                }),
            Err(_) => Vec::new(),
        };
        Self { entries, path }
    }

    /// Persist the recents to disk (pretty TOML, owner-only permissions).
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let file = RecentFile {
            recent: self.entries.clone(),
        };
        let text = toml::to_string_pretty(&file).context("serializing recents")?;
        write_private(&self.path, text.as_bytes())
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }

    /// Record a successful connection: an existing entry for the same
    /// `(host, username, port)` is replaced and the entry moves to the front;
    /// the list is capped at [`MAX_RECENT`].
    pub fn record(&mut self, entry: RecentEntry) {
        self.entries.retain(|existing| {
            !(existing.host == entry.host
                && existing.username == entry.username
                && existing.port == entry.port)
        });
        self.entries.insert(0, entry);
        self.entries.truncate(MAX_RECENT);
    }

    /// Remove the entry at `index`, if any.
    pub fn remove(&mut self, index: usize) {
        if index < self.entries.len() {
            self.entries.remove(index);
        }
    }
}

/// Seconds since the Unix epoch right now (0 if the clock is before it).
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Human-friendly relative time, e.g. `just now`, `5m ago`, `2h ago`,
/// `3d ago`, `2w ago`, `4mo ago`. Future timestamps read as `just now`.
pub fn relative_time(now_unix: u64, then_unix: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    const WEEK: u64 = 7 * DAY;
    const MONTH: u64 = 30 * DAY;

    let elapsed = now_unix.saturating_sub(then_unix);
    if elapsed < MINUTE {
        "just now".to_string()
    } else if elapsed < HOUR {
        format!("{}m ago", elapsed / MINUTE)
    } else if elapsed < DAY {
        format!("{}h ago", elapsed / HOUR)
    } else if elapsed < WEEK {
        format!("{}d ago", elapsed / DAY)
    } else if elapsed < MONTH {
        format!("{}w ago", elapsed / WEEK)
    } else {
        format!("{}mo ago", elapsed / MONTH)
    }
}

/// Write the recents file; on unix the file is created with 0o600 since it
/// mirrors connection metadata that users may consider private.
#[cfg(unix)]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(bytes))
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(host: &str, username: &str, port: u16, at: u64) -> RecentEntry {
        RecentEntry {
            profile_name: format!("{username}@{host}"),
            host: host.into(),
            username: username.into(),
            port,
            connected_at_unix: at,
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("aetherium-recents-{tag}-{}", std::process::id()))
    }

    #[test]
    fn toml_round_trip() {
        let dir = temp_dir("round-trip");
        let path = dir.join("recents.toml");

        let mut store = RecentStore::load_from(path.clone());
        assert!(store.entries.is_empty());
        store.record(entry("dev.example.com", "alice", 22, 1_000));
        store.record(entry("10.0.0.5", "deploy", 2222, 2_000));
        store.save().expect("save recents");

        let loaded = RecentStore::load_from(path.clone());
        assert_eq!(loaded.entries, store.entries);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dedup_moves_entry_to_front() {
        let mut store = RecentStore::load_from(temp_dir("dedup").join("recents.toml"));
        store.record(entry("a.example.com", "alice", 22, 1_000));
        store.record(entry("b.example.com", "bob", 22, 2_000));
        store.record(entry("a.example.com", "alice", 22, 3_000));

        assert_eq!(store.entries.len(), 2);
        assert_eq!(store.entries[0].host, "a.example.com");
        assert_eq!(store.entries[0].connected_at_unix, 3_000);
        assert_eq!(store.entries[1].host, "b.example.com");

        // Same host but a different user or port is a distinct entry.
        store.record(entry("a.example.com", "root", 22, 4_000));
        store.record(entry("a.example.com", "alice", 2222, 5_000));
        assert_eq!(store.entries.len(), 4);
    }

    #[test]
    fn capped_at_ten() {
        let mut store = RecentStore::load_from(temp_dir("cap").join("recents.toml"));
        for i in 0..15u64 {
            store.record(entry(&format!("host-{i}.example.com"), "alice", 22, 1_000 + i));
        }
        assert_eq!(store.entries.len(), 10);
        // Most recent first, oldest evicted.
        assert_eq!(store.entries[0].host, "host-14.example.com");
        assert_eq!(store.entries[9].host, "host-5.example.com");
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("mode");
        let path = dir.join("recents.toml");
        let mut store = RecentStore::load_from(path.clone());
        store.record(entry("dev.example.com", "alice", 22, 1_000));
        store.save().expect("save recents");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_is_empty_store() {
        let dir = temp_dir("corrupt");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("recents.toml");
        fs::write(&path, "this is [not valid toml").unwrap();
        let store = RecentStore::load_from(path.clone());
        assert!(store.entries.is_empty());
        // The corrupt file is preserved next to the original.
        let backup = PathBuf::from(format!("{}.bak", path.display()));
        assert_eq!(fs::read_to_string(&backup).unwrap(), "this is [not valid toml");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn relative_time_ranges() {
        let now = 10_000_000;
        assert_eq!(relative_time(now, now), "just now");
        assert_eq!(relative_time(now, now - 30), "just now");
        assert_eq!(relative_time(now, now - 60), "1m ago");
        assert_eq!(relative_time(now, now - 5 * 60), "5m ago");
        assert_eq!(relative_time(now, now - 59 * 60), "59m ago");
        assert_eq!(relative_time(now, now - 60 * 60), "1h ago");
        assert_eq!(relative_time(now, now - 2 * 3600), "2h ago");
        assert_eq!(relative_time(now, now - 23 * 3600), "23h ago");
        assert_eq!(relative_time(now, now - 24 * 3600), "1d ago");
        assert_eq!(relative_time(now, now - 3 * 86_400), "3d ago");
        assert_eq!(relative_time(now, now - 7 * 86_400), "1w ago");
        assert_eq!(relative_time(now, now - 14 * 86_400), "2w ago");
        assert_eq!(relative_time(now, now - 60 * 86_400), "2mo ago");
        // Clock skew / future timestamps never panic or look weird.
        assert_eq!(relative_time(now - 100, now), "just now");
    }
}
