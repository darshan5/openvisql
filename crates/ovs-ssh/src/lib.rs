//! SSH tunnels: a local TCP port forwarded to a database host through an SSH server, the way
//! `ssh -L` works. Database drivers then connect to `127.0.0.1:<local port>`.
//!
//! Host keys are checked trust-on-first-use: the caller passes the fingerprint it saw last
//! time (if any) and the tunnel refuses to connect when the server presents a different one.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use russh::client::{self, Config, Handle};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate, load_secret_key};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshAuth {
    Password(String),
    PrivateKey {
        path: PathBuf,
        passphrase: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshSettings {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
}

#[derive(Debug, Error)]
pub enum SshError {
    #[error("could not reach the SSH server: {0}")]
    Connect(String),
    #[error(
        "the SSH server's host key changed (expected {expected}, got {actual}); refusing to \
         connect in case the connection is being intercepted"
    )]
    HostKeyChanged { expected: String, actual: String },
    #[error("SSH authentication failed for user {0}")]
    AuthFailed(String),
    #[error("could not read the private key: {0}")]
    Key(String),
    #[error("could not open a local port for the tunnel: {0}")]
    Listen(String),
}

/// Checks the server's host key against the expected fingerprint and records what it saw.
struct HostKeyCheck {
    expected: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
}

impl client::Handler for HostKeyCheck {
    type Error = russh::Error;

    fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send {
        let fingerprint = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => {
                key.fingerprint(HashAlg::Sha256).to_string()
            }
            PublicKeyOrCertificate::Certificate(cert) => {
                cert.public_key().fingerprint(HashAlg::Sha256).to_string()
            }
        };
        let accepted = self.expected.as_ref().is_none_or(|e| *e == fingerprint);
        *self.seen.lock().unwrap_or_else(PoisonError::into_inner) = Some(fingerprint);
        std::future::ready(Ok(accepted))
    }
}

/// An open tunnel. Dropping it stops forwarding and closes the SSH connection.
pub struct Tunnel {
    local_port: u16,
    fingerprint: String,
    handle: Arc<Handle<HostKeyCheck>>,
    forwarder: JoinHandle<()>,
}

impl Tunnel {
    /// Connects to the SSH server and starts forwarding a local port to `target:target_port`
    /// (as seen from the SSH server). `known_fingerprint` is the host key fingerprint seen on a
    /// previous connection, if any.
    pub async fn open(
        settings: &SshSettings,
        target: &str,
        target_port: u16,
        known_fingerprint: Option<String>,
    ) -> Result<Self, SshError> {
        let seen = Arc::new(Mutex::new(None));
        let check = HostKeyCheck {
            expected: known_fingerprint.clone(),
            seen: seen.clone(),
        };
        let config = Arc::new(Config {
            nodelay: true,
            keepalive_interval: Some(std::time::Duration::from_secs(30)),
            ..Config::default()
        });
        let connected =
            client::connect(config, (settings.host.as_str(), settings.port), check).await;
        let fingerprint = seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap_or_default();
        let mut handle = match connected {
            Ok(handle) => handle,
            Err(err) => {
                return Err(match known_fingerprint {
                    Some(expected) if !fingerprint.is_empty() && expected != fingerprint => {
                        SshError::HostKeyChanged {
                            expected,
                            actual: fingerprint,
                        }
                    }
                    _ => SshError::Connect(err.to_string()),
                });
            }
        };

        let auth = match &settings.auth {
            SshAuth::Password(password) => {
                handle
                    .authenticate_password(settings.user.clone(), password.clone())
                    .await
            }
            SshAuth::PrivateKey { path, passphrase } => {
                let key = load_secret_key(path, passphrase.as_deref())
                    .map_err(|e| SshError::Key(e.to_string()))?;
                let hash = handle
                    .best_supported_rsa_hash()
                    .await
                    .map_err(|e| SshError::Connect(e.to_string()))?
                    .flatten();
                handle
                    .authenticate_publickey(
                        settings.user.clone(),
                        PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                    )
                    .await
            }
        }
        .map_err(|e| SshError::Connect(e.to_string()))?;
        if !auth.success() {
            return Err(SshError::AuthFailed(settings.user.clone()));
        }

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| SshError::Listen(e.to_string()))?;
        let local_port = listener
            .local_addr()
            .map_err(|e| SshError::Listen(e.to_string()))?
            .port();
        let handle = Arc::new(handle);
        let forwarder = tokio::spawn(forward(
            listener,
            handle.clone(),
            target.to_owned(),
            target_port,
        ));
        Ok(Self {
            local_port,
            fingerprint,
            handle,
            forwarder,
        })
    }

    /// The port on 127.0.0.1 that leads to the target.
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// SHA-256 fingerprint of the SSH server's host key, e.g. `SHA256:...`.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// False once the SSH connection has dropped; the tunnel must then be reopened.
    pub fn is_alive(&self) -> bool {
        !self.handle.is_closed() && !self.forwarder.is_finished()
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.forwarder.abort();
    }
}

/// Accepts local connections and pipes each through its own SSH channel.
async fn forward(
    listener: TcpListener,
    handle: Arc<Handle<HostKeyCheck>>,
    target: String,
    target_port: u16,
) {
    while let Ok((mut socket, peer)) = listener.accept().await {
        if handle.is_closed() {
            break;
        }
        let ssh = handle.clone();
        let target = target.clone();
        tokio::spawn(async move {
            let Ok(channel) = ssh
                .channel_open_direct_tcpip(
                    target,
                    u32::from(target_port),
                    peer.ip().to_string(),
                    u32::from(peer.port()),
                )
                .await
            else {
                return;
            };
            let mut stream = channel.into_stream();
            // Either side closing ends the pipe; errors just close this one connection.
            let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
        });
    }
}
