use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Database engines OpenViSQL can talk to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    /// MySQL and MariaDB share one wire protocol and driver.
    #[default]
    MySql,
}

impl Engine {
    pub fn default_port(self) -> u16 {
        match self {
            Self::MySql => 3306,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::MySql => "MySQL / MariaDB",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SslMode {
    /// Plain TCP.
    #[default]
    Disabled,
    /// TLS, but the server certificate is not verified.
    Preferred,
    /// TLS with full certificate and hostname verification.
    Required,
}

/// Where a saved password is kept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PasswordStore {
    /// The OS credential store (macOS Keychain, Windows Credential Manager).
    #[default]
    Keychain,
    /// An encrypted file in the app's config directory; avoids keychain prompts.
    Local,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SshAuthMethod {
    #[default]
    Password,
    PrivateKey,
}

/// Connecting through an SSH tunnel: the database host and port are then as seen from the SSH
/// server (often `127.0.0.1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SshConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuthMethod,
    /// Private key file, for `SshAuthMethod::PrivateKey`.
    pub key_path: String,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            port: 22,
            user: String::new(),
            auth: SshAuthMethod::default(),
            key_path: String::new(),
        }
    }
}

/// A saved connection. Passwords are never stored here; see `ovs-store`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionConfig {
    pub id: Uuid,
    pub name: String,
    pub engine: Engine,
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Databases to show in the object browser, separated by `;`. Empty shows all.
    pub databases: String,
    pub save_password: bool,
    /// Where the password is saved when `save_password` is set.
    pub password_store: PasswordStore,
    pub ssl: SslMode,
    /// Optional path to a PEM CA bundle used when `ssl` is `Required`.
    pub ssl_ca: Option<String>,
    pub compress: bool,
    /// Statements run right after connecting, separated by `;`.
    pub init_commands: String,
    pub connect_timeout_secs: u64,
    /// Ask before running UPDATE/DELETE without a WHERE clause.
    pub safe_updates: bool,
    pub ssh: SshConfig,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        let engine = Engine::default();
        Self {
            id: Uuid::new_v4(),
            name: "New connection".to_owned(),
            engine,
            host: "localhost".to_owned(),
            port: engine.default_port(),
            user: "root".to_owned(),
            databases: String::new(),
            save_password: true,
            password_store: PasswordStore::default(),
            ssl: SslMode::default(),
            ssl_ca: None,
            compress: false,
            init_commands: String::new(),
            connect_timeout_secs: 10,
            safe_updates: true,
            ssh: SshConfig::default(),
        }
    }
}

impl ConnectionConfig {
    /// The database filter as a list, with blanks removed.
    pub fn database_filter(&self) -> Vec<&str> {
        self.databases
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// `user@host:port`, used in window titles and the status bar.
    pub fn endpoint(&self) -> String {
        format!("{}@{}:{}", self.user, self.host, self.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_filter_ignores_blanks() {
        let cfg = ConnectionConfig {
            databases: " app ;; logs;".into(),
            ..ConnectionConfig::default()
        };
        assert_eq!(cfg.database_filter(), ["app", "logs"]);
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let cfg: ConnectionConfig =
            toml::from_str("name = \"prod\"\nhost = \"db.example\"").unwrap();
        assert_eq!(cfg.name, "prod");
        assert_eq!(cfg.port, 3306);
        assert_eq!(cfg.user, "root");
    }
}
