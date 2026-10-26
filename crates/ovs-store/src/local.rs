//! Passwords encrypted in a file next to the other settings, for users who'd rather not use
//! (or be prompted by) the OS keychain.
//!
//! A random 256-bit key is created on first use in `secret.key`, readable only by the current
//! user, and passwords are sealed with XChaCha20-Poly1305 into `secrets.toml`, each bound to its
//! connection id. This keeps passwords out of `connections.toml` and safe from anyone who only
//! gets the settings files; it does not protect against someone who can read the user's files,
//! since the key sits beside them.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chacha20poly1305::XChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use serde::{Deserialize, Serialize};

use crate::{Result, StoreError, read_toml, write_toml};

const KEY_FILE: &str = "secret.key";
const SECRETS_FILE: &str = "secrets.toml";
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

#[derive(Debug, Default, Serialize, Deserialize)]
struct SecretsFile {
    /// Connection id → base64(nonce ‖ ciphertext).
    #[serde(default)]
    passwords: BTreeMap<String, String>,
}

pub(crate) struct LocalSecrets<'a> {
    dir: &'a Path,
}

fn invalid(path: &Path, message: &str) -> StoreError {
    StoreError::Io {
        path: path.to_owned(),
        source: io::Error::new(io::ErrorKind::InvalidData, message.to_owned()),
    }
}

fn random<const N: usize>(path: &Path) -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|e| invalid(path, &format!("no randomness: {e}")))?;
    Ok(bytes)
}

impl<'a> LocalSecrets<'a> {
    pub(crate) fn new(dir: &'a Path) -> Self {
        Self { dir }
    }

    fn path(&self, file: &str) -> PathBuf {
        self.dir.join(file)
    }

    /// The encryption key, created on first use.
    fn cipher(&self, create: bool) -> Result<Option<XChaCha20Poly1305>> {
        let path = self.path(KEY_FILE);
        let key = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound && !create => return Ok(None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let key: [u8; KEY_LEN] = random(&path)?;
                write_private(&path, &key)?;
                key.to_vec()
            }
            Err(source) => return Err(StoreError::Io { path, source }),
        };
        XChaCha20Poly1305::new_from_slice(&key)
            .map(Some)
            .map_err(|_| invalid(&path, "the key file is damaged"))
    }

    fn load(&self) -> Result<SecretsFile> {
        read_toml(&self.path(SECRETS_FILE))
    }

    /// `key` names the secret in the file; `aad` binds the ciphertext to it.
    pub(crate) fn get(&self, key: &str, aad: &[u8]) -> Result<Option<String>> {
        let file = self.load()?;
        let Some(sealed) = file.passwords.get(key) else {
            return Ok(None);
        };
        let path = self.path(SECRETS_FILE);
        let Some(cipher) = self.cipher(false)? else {
            return Err(invalid(
                &path,
                "the key file is missing; re-enter the password",
            ));
        };
        let bytes = B64
            .decode(sealed)
            .map_err(|_| invalid(&path, "a stored password is damaged"))?;
        if bytes.len() < NONCE_LEN {
            return Err(invalid(&path, "a stored password is damaged"));
        }
        let (nonce, ciphertext) = bytes.split_at(NONCE_LEN);
        let plain = cipher
            .decrypt(
                nonce.try_into().map_err(|_| invalid(&path, "bad nonce"))?,
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|_| invalid(&path, "a stored password can't be decrypted; re-enter it"))?;
        String::from_utf8(plain)
            .map(Some)
            .map_err(|_| invalid(&path, "a stored password is damaged"))
    }

    pub(crate) fn set(&self, key: &str, aad: &[u8], password: &str) -> Result<()> {
        let path = self.path(SECRETS_FILE);
        let cipher = self.cipher(true)?.expect("cipher is created on demand");
        let nonce: [u8; NONCE_LEN] = random(&path)?;
        let ciphertext = cipher
            .encrypt(
                (&nonce).into(),
                Payload {
                    msg: password.as_bytes(),
                    aad,
                },
            )
            .map_err(|_| invalid(&path, "encryption failed"))?;
        let mut sealed = nonce.to_vec();
        sealed.extend_from_slice(&ciphertext);
        let mut file = self.load()?;
        file.passwords.insert(key.to_owned(), B64.encode(sealed));
        write_toml(&path, &file)
    }

    pub(crate) fn delete(&self, key: &str) -> Result<()> {
        let mut file = self.load()?;
        if file.passwords.remove(key).is_some() {
            write_toml(&self.path(SECRETS_FILE), &file)?;
        }
        Ok(())
    }
}

/// Writes a file only the current user can read. On Windows the per-user profile directory
/// already restricts access.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let io_err = |source| StoreError::Io {
        path: path.to_owned(),
        source,
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_err)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(io_err)?;
        file.write_all(bytes).map_err(io_err)
    }
    #[cfg(not(unix))]
    {
        fs::write(path, bytes).map_err(io_err)
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    fn get(s: &LocalSecrets<'_>, id: Uuid) -> Result<Option<String>> {
        s.get(&id.to_string(), id.as_bytes())
    }

    fn set(s: &LocalSecrets<'_>, id: Uuid, password: &str) -> Result<()> {
        s.set(&id.to_string(), id.as_bytes(), password)
    }

    #[test]
    fn round_trip_and_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = LocalSecrets::new(dir.path());
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        assert_eq!(get(&secrets, a).unwrap(), None);

        set(&secrets, a, "s3cret ü").unwrap();
        set(&secrets, b, "other").unwrap();
        assert_eq!(get(&secrets, a).unwrap().as_deref(), Some("s3cret ü"));
        assert_eq!(get(&secrets, b).unwrap().as_deref(), Some("other"));

        // Nothing readable on disk.
        let text = fs::read_to_string(dir.path().join(SECRETS_FILE)).unwrap();
        assert!(!text.contains("s3cret") && !text.contains("other"));

        secrets.delete(&a.to_string()).unwrap();
        assert_eq!(get(&secrets, a).unwrap(), None);
        assert_eq!(get(&secrets, b).unwrap().as_deref(), Some("other"));
    }

    #[test]
    fn ciphertext_is_bound_to_its_connection() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = LocalSecrets::new(dir.path());
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        set(&secrets, a, "for-a").unwrap();
        // Copy a's sealed password under b's id: decryption must fail, not return a's password.
        let mut file = secrets.load().unwrap();
        let sealed = file.passwords[&a.to_string()].clone();
        file.passwords.insert(b.to_string(), sealed);
        write_toml(&dir.path().join(SECRETS_FILE), &file).unwrap();
        assert!(get(&secrets, b).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        set(&LocalSecrets::new(dir.path()), Uuid::new_v4(), "x").unwrap();
        let mode = fs::metadata(dir.path().join(KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
