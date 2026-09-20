//! Small encryption helpers for sensitive on-disk data.
//!
//! A single AES-256-GCM key is generated on first use and stored in
//! `~/.config/aetherium/key` as base64. Passwords and key passphrases are
//! encrypted with that key before being written to `profiles.toml`.

use std::path::PathBuf;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context as _, Result};
use base64::Engine as _;
use rand::rngs::OsRng;
use rand::RngCore as _;

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;

pub fn config_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("AETHERIUM_CONFIG_DIR") {
        return PathBuf::from(override_dir);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("aetherium")
}

fn read_key_file(path: &std::path::Path) -> Result<Option<[u8; KEY_LEN]>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .context("key file is not valid base64")?;
    if bytes.len() != KEY_LEN {
        anyhow::bail!("encryption key must be {} bytes, got {}", KEY_LEN, bytes.len());
    }
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&bytes);
    Ok(Some(key))
}

fn write_key_file(path: &std::path::Path, key: &[u8; KEY_LEN]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(key);
    write_private(path, encoded.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Return the existing AES-256-GCM key or generate and persist a new one in
/// `dir`. Useful for tests and for stores that live outside the default
/// config directory.
pub fn get_or_create_encryption_key_at(dir: &std::path::Path) -> Result<[u8; KEY_LEN]> {
    let path = dir.join("key");
    if let Some(key) = read_key_file(&path)? {
        return Ok(key);
    }
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let mut key = [0u8; KEY_LEN];
    OsRng.fill_bytes(&mut key);
    write_key_file(&path, &key)?;
    Ok(key)
}

/// Encrypt a UTF-8 string; returns base64(nonce || ciphertext).
pub fn encrypt(plaintext: &str, key: &[u8; KEY_LEN]) -> Result<String> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| anyhow::anyhow!("invalid encryption key length: {e}"))?;
    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plaintext.as_bytes())
        .map_err(|e| anyhow::anyhow!("encryption failed: {e}"))?;
    let mut payload = nonce_bytes.to_vec();
    payload.extend_from_slice(&ciphertext);
    Ok(base64::engine::general_purpose::STANDARD.encode(payload))
}

/// Decrypt a string produced by [`encrypt`].
pub fn decrypt(ciphertext: &str, key: &[u8; KEY_LEN]) -> Result<String> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| anyhow::anyhow!("invalid encryption key length: {e}"))?;
    let payload = base64::engine::general_purpose::STANDARD
        .decode(ciphertext)
        .context("encrypted value is not valid base64")?;
    if payload.len() < NONCE_LEN + 1 {
        anyhow::bail!("encrypted value is too short");
    }
    let (nonce_bytes, ciphertext) = payload.split_at(NONCE_LEN);
    let plaintext = cipher
        .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
        .map_err(|e| anyhow::anyhow!("decryption failed: {e}"))?;
    String::from_utf8(plaintext).context("decrypted value is not valid UTF-8")
}

/// Write a file that should not be world-readable.
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
    std::fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("aetherium-crypto-test-{}", std::process::id()));
        let key = get_or_create_encryption_key_at(&dir).expect("key");
        let original = "my s3cret p@ssw0rd";
        let sealed = encrypt(original, &key).expect("encrypt");
        assert_ne!(sealed, original);
        let opened = decrypt(&sealed, &key).expect("decrypt");
        assert_eq!(opened, original);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
