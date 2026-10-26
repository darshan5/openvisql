//! Persistence for things the user expects to survive a restart.
//!
//! - Connections live in `connections.toml` in the platform config directory.
//! - Passwords live either in the OS credential store (macOS Keychain, Windows Credential
//!   Manager) or encrypted in `secrets.toml`, as each connection chooses; never in plain text.
//!   The keychain is only touched for connections that use it.
//! - Query history lives in `history.toml`, capped at [`HISTORY_LIMIT`] entries.
//! - Open connection tabs and their query tabs live in `session.toml`, so the app reopens
//!   where the user left off.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ovs_core::{ConnectionConfig, PasswordStore};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod keychain;
mod local;
use uuid::Uuid;

const APP_DIR: &str = "openvisql";
pub const HISTORY_LIMIT: usize = 500;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("could not access {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("{path} is not valid: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("could not serialise settings: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("credential store: {0}")]
    Keyring(#[from] keyring::Error),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Platform config directory for the app, e.g. `~/Library/Application Support/openvisql`.
pub fn default_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConnectionsFile {
    #[serde(default, rename = "connection")]
    connections: Vec<ConnectionConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Unix timestamp in seconds.
    pub at: u64,
    pub connection: String,
    pub database: Option<String>,
    pub sql: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct HistoryFile {
    #[serde(default, rename = "entry")]
    entries: Vec<HistoryEntry>,
}

/// One query tab as it should reopen.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedQueryTab {
    pub title: String,
    /// The `.sql` file the tab was opened from or saved to.
    pub path: Option<String>,
    pub database: Option<String>,
    pub sql: String,
}

/// One connection tab of the main window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedConnectionTab {
    /// The saved connection this tab was connected to.
    pub connection: Option<Uuid>,
    #[serde(rename = "tab")]
    pub tabs: Vec<SavedQueryTab>,
    pub active_tab: usize,
}

/// Everything needed to restore the main window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    #[serde(rename = "connection")]
    pub connections: Vec<SavedConnectionTab>,
    pub active: usize,
}

/// A secret belonging to a connection.
#[derive(Clone, Copy)]
enum Secret {
    Database(Uuid),
    Ssh(Uuid),
}

impl Secret {
    /// Keychain account and local-file key. Database passwords use the bare id, as they did
    /// before SSH secrets existed.
    fn name(self) -> String {
        match self {
            Self::Database(id) => id.to_string(),
            Self::Ssh(id) => format!("{id}:ssh"),
        }
    }

    /// What a locally encrypted secret is bound to.
    fn aad(self) -> Vec<u8> {
        match self {
            Self::Database(id) => id.as_bytes().to_vec(),
            Self::Ssh(_) => self.name().into_bytes(),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct KnownHosts {
    /// `host:port` → SHA-256 fingerprint.
    #[serde(default)]
    hosts: std::collections::BTreeMap<String, String>,
}

/// File-backed store rooted at one directory. Tests point it at a temp dir.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Default for Store {
    fn default() -> Self {
        Self::new(default_dir())
    }
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn path(&self, file: &str) -> PathBuf {
        self.dir.join(file)
    }

    pub fn load_connections(&self) -> Result<Vec<ConnectionConfig>> {
        let file: ConnectionsFile = read_toml(&self.path("connections.toml"))?;
        Ok(file.connections)
    }

    pub fn save_connections(&self, connections: &[ConnectionConfig]) -> Result<()> {
        let file = ConnectionsFile {
            connections: connections.to_vec(),
        };
        write_toml(&self.path("connections.toml"), &file)
    }

    /// The saved password of connection `id` from `store`, if any.
    pub fn password(&self, id: Uuid, store: PasswordStore) -> Result<Option<String>> {
        self.secret(Secret::Database(id), store)
    }

    pub fn set_password(&self, id: Uuid, store: PasswordStore, password: &str) -> Result<()> {
        self.set_secret(Secret::Database(id), store, password)
    }

    pub fn delete_password(&self, id: Uuid, store: PasswordStore) -> Result<()> {
        self.delete_secret(Secret::Database(id), store)
    }

    /// The saved SSH password or key passphrase of connection `id`.
    pub fn ssh_secret(&self, id: Uuid, store: PasswordStore) -> Result<Option<String>> {
        self.secret(Secret::Ssh(id), store)
    }

    pub fn set_ssh_secret(&self, id: Uuid, store: PasswordStore, secret: &str) -> Result<()> {
        self.set_secret(Secret::Ssh(id), store, secret)
    }

    pub fn delete_ssh_secret(&self, id: Uuid, store: PasswordStore) -> Result<()> {
        self.delete_secret(Secret::Ssh(id), store)
    }

    fn secret(&self, secret: Secret, store: PasswordStore) -> Result<Option<String>> {
        match store {
            PasswordStore::Keychain => keychain::get_password(&secret.name()),
            PasswordStore::Local => {
                local::LocalSecrets::new(&self.dir).get(&secret.name(), &secret.aad())
            }
        }
    }

    fn set_secret(&self, secret: Secret, store: PasswordStore, value: &str) -> Result<()> {
        match store {
            PasswordStore::Keychain => keychain::set_password(&secret.name(), value),
            PasswordStore::Local => {
                local::LocalSecrets::new(&self.dir).set(&secret.name(), &secret.aad(), value)
            }
        }
    }

    fn delete_secret(&self, secret: Secret, store: PasswordStore) -> Result<()> {
        match store {
            PasswordStore::Keychain => keychain::delete_password(&secret.name()),
            PasswordStore::Local => local::LocalSecrets::new(&self.dir).delete(&secret.name()),
        }
    }

    /// The SSH host key fingerprint seen on earlier connections to `host:port`.
    pub fn known_host(&self, host: &str, port: u16) -> Result<Option<String>> {
        let file: KnownHosts = read_toml(&self.path("known_hosts.toml"))?;
        Ok(file.hosts.get(&format!("{host}:{port}")).cloned())
    }

    pub fn remember_host(&self, host: &str, port: u16, fingerprint: &str) -> Result<()> {
        let path = self.path("known_hosts.toml");
        let mut file: KnownHosts = read_toml(&path)?;
        file.hosts
            .insert(format!("{host}:{port}"), fingerprint.to_owned());
        write_toml(&path, &file)
    }

    pub fn load_session(&self) -> Result<Session> {
        read_toml(&self.path("session.toml"))
    }

    pub fn save_session(&self, session: &Session) -> Result<()> {
        write_toml(&self.path("session.toml"), session)
    }

    pub fn load_history(&self) -> Result<Vec<HistoryEntry>> {
        let file: HistoryFile = read_toml(&self.path("history.toml"))?;
        Ok(file.entries)
    }

    /// Appends to history, dropping the oldest entries beyond [`HISTORY_LIMIT`].
    pub fn append_history(&self, entry: HistoryEntry) -> Result<()> {
        let mut entries = self.load_history()?;
        entries.push(entry);
        if entries.len() > HISTORY_LIMIT {
            entries.drain(..entries.len() - HISTORY_LIMIT);
        }
        write_toml(&self.path("history.toml"), &HistoryFile { entries })
    }
}

pub(crate) fn read_toml<T: Default + for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    match fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(|source| StoreError::Parse {
            path: path.to_owned(),
            source,
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(source) => Err(StoreError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Writes via a temp file and rename so a crash never leaves a half-written settings file.
pub(crate) fn write_toml<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let io_err = |source| StoreError::Io {
        path: path.to_owned(),
        source,
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_err)?;
    }
    let text = toml::to_string_pretty(value)?;
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, text).map_err(io_err)?;
    fs::rename(&tmp, path).map_err(io_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connections_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        assert_eq!(
            store.load_connections().unwrap(),
            [] as [ovs_core::ConnectionConfig; 0]
        );

        let cfg = ConnectionConfig {
            name: "local".into(),
            port: 3307,
            ..ConnectionConfig::default()
        };
        store.save_connections(std::slice::from_ref(&cfg)).unwrap();
        assert_eq!(store.load_connections().unwrap(), [cfg]);
    }

    #[test]
    fn session_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        assert_eq!(store.load_session().unwrap(), Session::default());

        let session = Session {
            connections: vec![SavedConnectionTab {
                connection: Some(Uuid::new_v4()),
                tabs: vec![
                    SavedQueryTab {
                        title: "Orders".into(),
                        path: Some("/tmp/orders.sql".into()),
                        database: Some("shop".into()),
                        sql: "SELECT *\nFROM orders;".into(),
                    },
                    SavedQueryTab::default(),
                ],
                active_tab: 1,
            }],
            active: 0,
        };
        store.save_session(&session).unwrap();
        assert_eq!(store.load_session().unwrap(), session);
    }

    #[test]
    fn ssh_secrets_and_known_hosts() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let id = Uuid::new_v4();
        store.set_password(id, PasswordStore::Local, "db").unwrap();
        store
            .set_ssh_secret(id, PasswordStore::Local, "ssh")
            .unwrap();
        assert_eq!(
            store.password(id, PasswordStore::Local).unwrap().as_deref(),
            Some("db")
        );
        assert_eq!(
            store
                .ssh_secret(id, PasswordStore::Local)
                .unwrap()
                .as_deref(),
            Some("ssh")
        );
        store.delete_ssh_secret(id, PasswordStore::Local).unwrap();
        assert_eq!(store.ssh_secret(id, PasswordStore::Local).unwrap(), None);
        assert_eq!(
            store.password(id, PasswordStore::Local).unwrap().as_deref(),
            Some("db")
        );

        assert_eq!(store.known_host("db.example", 22).unwrap(), None);
        store.remember_host("db.example", 22, "SHA256:abc").unwrap();
        assert_eq!(
            store.known_host("db.example", 22).unwrap().as_deref(),
            Some("SHA256:abc")
        );
    }

    #[test]
    fn history_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        for i in 0..HISTORY_LIMIT + 3 {
            store
                .append_history(HistoryEntry {
                    at: i as u64,
                    connection: "c".into(),
                    database: None,
                    sql: format!("select {i}"),
                })
                .unwrap();
        }
        let history = store.load_history().unwrap();
        assert_eq!(history.len(), HISTORY_LIMIT);
        assert_eq!(history[0].sql, "select 3");
    }

    #[test]
    fn corrupt_file_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("connections.toml"), "not = [valid").unwrap();
        let err = Store::new(dir.path()).load_connections().unwrap_err();
        assert!(matches!(err, StoreError::Parse { .. }));
    }
}
