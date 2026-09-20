//! Connection profile model and TOML persistence.
//!
//! Profiles are stored at `~/.config/aetherium/profiles.toml`.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// How to authenticate to the SSH server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthMethod {
    /// Password authentication.
    Password { password: String },
    /// Public key authentication with a key file on disk.
    KeyFile {
        path: PathBuf,
        passphrase: Option<String>,
    },
    /// Authenticate through the running ssh-agent (SSH_AUTH_SOCK).
    Agent,
}

/// A saved SSH connection profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            name: String::new(),
            host: String::new(),
            port: 22,
            username: String::new(),
            auth: AuthMethod::Password { password: String::new() },
        }
    }
}

impl Profile {
    /// `user@host:port` one-line summary used in the UI.
    pub fn summary(&self) -> String {
        format!("{}@{}:{}", self.username, self.host, self.port)
    }
}

/// On-disk representation of the profile file.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ProfileFile {
    #[serde(default)]
    profiles: Vec<Profile>,
}

/// The collection of profiles plus the path they persist to.
pub struct ProfileStore {
    pub profiles: Vec<Profile>,
    path: PathBuf,
}

impl ProfileStore {
    /// Default location: `~/.config/aetherium/profiles.toml`.
    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("aetherium")
            .join("profiles.toml")
    }

    /// Load from the default location, creating an empty store (and parent
    /// directory) if the file is missing.
    pub fn load() -> Self {
        Self::load_from(Self::default_path())
    }

    /// Load from an explicit path. Missing files yield an empty store.
    pub fn load_from(path: PathBuf) -> Self {
        let profiles = match fs::read_to_string(&path) {
            Ok(text) => toml::from_str::<ProfileFile>(&text)
                .map(|file| file.profiles)
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
        Self { profiles, path }
    }

    /// Persist the profiles to disk (pretty TOML).
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let file = ProfileFile {
            profiles: self.profiles.clone(),
        };
        let text = toml::to_string_pretty(&file).context("serializing profiles")?;
        write_private(&self.path, text.as_bytes())
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }

    /// Insert a new profile or replace the one at `index`.
    pub fn upsert(&mut self, index: Option<usize>, profile: Profile) {
        match index {
            Some(i) if i < self.profiles.len() => self.profiles[i] = profile,
            _ => self.profiles.push(profile),
        }
    }

    /// Remove the profile at `index`, if any.
    pub fn remove(&mut self, index: usize) {
        if index < self.profiles.len() {
            self.profiles.remove(index);
        }
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

/// Write the profiles file; on unix the file is created with 0o600 since it
/// may contain passwords/passphrases.
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

    fn sample_profiles() -> Vec<Profile> {
        vec![
            Profile {
                name: "dev box".into(),
                host: "dev.example.com".into(),
                port: 22,
                username: "alice".into(),
                auth: AuthMethod::Password { password: "s3cret".into() },
            },
            Profile {
                name: "prod".into(),
                host: "10.0.0.5".into(),
                port: 2222,
                username: "deploy".into(),
                auth: AuthMethod::KeyFile {
                    path: PathBuf::from("/home/alice/.ssh/id_ed25519"),
                    passphrase: Some("phrase".into()),
                },
            },
            Profile {
                name: "agent box".into(),
                host: "agent.example.com".into(),
                port: 22,
                username: "bob".into(),
                auth: AuthMethod::Agent,
            },
            Profile {
                name: "key no passphrase".into(),
                host: "192.168.1.2".into(),
                port: 22,
                username: "root".into(),
                auth: AuthMethod::KeyFile {
                    path: PathBuf::from("~/.ssh/id_rsa"),
                    passphrase: None,
                },
            },
        ]
    }

    #[test]
    fn toml_round_trip() {
        let dir = std::env::temp_dir().join(format!("aetherium-test-{}", std::process::id()));
        let path = dir.join("profiles.toml");

        let mut store = ProfileStore::load_from(path.clone());
        assert!(store.profiles.is_empty());
        for profile in sample_profiles() {
            store.upsert(None, profile);
        }
        store.save().expect("save profiles");

        let loaded = ProfileStore::load_from(path.clone());
        assert_eq!(loaded.profiles, sample_profiles());

        // Update in place.
        let mut loaded = loaded;
        loaded.upsert(
            Some(0),
            Profile {
                name: "dev box 2".into(),
                ..sample_profiles()[0].clone()
            },
        );
        assert_eq!(loaded.profiles[0].name, "dev box 2");
        loaded.remove(1);
        assert_eq!(loaded.profiles.len(), 3);
        loaded.save().expect("re-save");
        let reloaded = ProfileStore::load_from(path.clone());
        assert_eq!(reloaded.profiles, loaded.profiles);

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("aetherium-mode-{}", std::process::id()));
        let path = dir.join("profiles.toml");
        let mut store = ProfileStore::load_from(path.clone());
        store.upsert(None, sample_profiles().into_iter().next().unwrap());
        store.save().expect("save profiles");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_empty_store() {
        let path = std::env::temp_dir().join("aetherium-does-not-exist/profiles.toml");
        let store = ProfileStore::load_from(path);
        assert!(store.profiles.is_empty());
    }

    #[test]
    fn corrupt_file_is_empty_store() {
        let dir = std::env::temp_dir().join(format!("aetherium-corrupt-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("profiles.toml");
        fs::write(&path, "this is [not valid toml").unwrap();
        let store = ProfileStore::load_from(path.clone());
        assert!(store.profiles.is_empty());
        // The corrupt file is preserved next to the original.
        let backup = PathBuf::from(format!("{}.bak", path.display()));
        assert_eq!(fs::read_to_string(&backup).unwrap(), "this is [not valid toml");
        let _ = fs::remove_dir_all(&dir);
    }
}
