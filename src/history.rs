//! Cross-session command history (roadmap 3.4): the last commands typed in
//! any terminal tab, persisted next to the profiles and recallable with
//! Shift+↑ / Shift+↓ in any later session — the remote shell's own history
//! only knows about its own sessions.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// Hard cap on remembered commands.
const MAX_COMMANDS: usize = 200;

#[derive(Default, Serialize, Deserialize)]
struct HistoryFile {
    #[serde(default)]
    commands: Vec<String>,
}

#[derive(Default)]
pub struct HistoryStore {
    commands: Vec<String>,
    path: PathBuf,
}

impl HistoryStore {
    /// Default location: `<config dir>/command_history.toml`.
    pub fn default_path() -> PathBuf {
        crate::crypto::config_dir().join("command_history.toml")
    }

    /// Load from the default location; missing or corrupt files yield an
    /// empty store (the corrupt file is left in place for recovery).
    pub fn load() -> Self {
        Self::load_from(Self::default_path())
    }

    /// Load from an explicit path (used by tests).
    pub fn load_from(path: PathBuf) -> Self {
        let commands = match fs::read_to_string(&path) {
            Ok(text) => toml::from_str::<HistoryFile>(&text)
                .map(|file| file.commands)
                .unwrap_or_else(|err| {
                    eprintln!(
                        "aetherium: failed to parse {}: {err}; starting with empty history",
                        path.display()
                    );
                    Vec::new()
                }),
            Err(_) => Vec::new(),
        };
        Self { commands, path }
    }

    pub fn save(&self) -> Result<()> {
        let file = HistoryFile {
            commands: self.commands.clone(),
        };
        let text =
            toml::to_string_pretty(&file).context("serializing command history")?;
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        // Write-then-rename so a crash mid-save cannot truncate the history.
        let tmp = self.path.with_extension("toml.tmp");
        fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("renaming to {}", self.path.display()))
    }

    /// Record a submitted command; empty commands and consecutive duplicates
    /// are dropped.
    pub fn push(&mut self, command: String) {
        let command = command.trim();
        if command.is_empty() || self.commands.last().is_some_and(|last| last == command) {
            return;
        }
        self.commands.push(command.to_string());
        if self.commands.len() > MAX_COMMANDS {
            self.commands.remove(0);
        }
    }

    pub fn commands(&self) -> &[String] {
        &self.commands
    }

    pub fn len(&self) -> usize {
        self.commands.len()
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("aetherium-history-test-{}-{}", tag, std::process::id()))
    }

    #[test]
    fn push_trims_dedupes_and_caps() {
        let mut store = HistoryStore::load_from(temp_path("cap"));
        store.push("  ls -la  ".to_string());
        assert_eq!(store.commands(), &["ls -la".to_string()]);
        // Consecutive duplicate collapses.
        store.push("ls -la".to_string());
        assert_eq!(store.len(), 1);
        // Non-consecutive duplicate is kept.
        store.push("pwd".to_string());
        store.push("ls -la".to_string());
        assert_eq!(store.len(), 3);
        // Empty and whitespace-only commands are dropped.
        store.push("   ".to_string());
        store.push(String::new());
        assert_eq!(store.len(), 3);
        // Cap: the oldest entries fall off the front.
        for i in 0..MAX_COMMANDS {
            store.push(format!("cmd-{i}"));
        }
        assert_eq!(store.len(), MAX_COMMANDS);
        assert_eq!(store.commands().first().map(String::as_str), Some("cmd-0"));
        store.push("one-more".to_string());
        assert_eq!(store.len(), MAX_COMMANDS);
        assert_eq!(store.commands().first().map(String::as_str), Some("cmd-1"));
    }

    #[test]
    fn round_trips_through_disk() {
        let path = temp_path("roundtrip");
        let mut store = HistoryStore::load_from(path.clone());
        store.push("ssh somewhere".to_string());
        store.push("tail -f log".to_string());
        store.save().expect("save");
        let loaded = HistoryStore::load_from(path.clone());
        assert_eq!(
            loaded.commands(),
            &["ssh somewhere".to_string(), "tail -f log".to_string()]
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn corrupt_file_is_empty_store() {
        let path = temp_path("corrupt");
        std::fs::write(&path, "this is [not toml").unwrap();
        let store = HistoryStore::load_from(path.clone());
        assert!(store.is_empty());
        let _ = std::fs::remove_file(path);
    }
}
