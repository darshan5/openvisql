//! Bridges GPUI's executor and tokio. Database drivers need a tokio reactor, while views run
//! on GPUI's own executor, so every driver call is spawned onto a shared tokio runtime and its
//! `JoinHandle` (an ordinary future) is awaited from GPUI tasks.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, OnceLock};

use ovs_core::{ConnectionConfig, Driver, Session, SshAuthMethod};
use ovs_ssh::{SshAuth, SshSettings, Tunnel};
use tokio::runtime::Runtime;
use tokio::sync::Mutex;
use uuid::Uuid;

/// A session shared between the view that owns it and the tokio tasks that use it.
pub type SharedSession = Arc<Mutex<Box<dyn Session>>>;

pub(crate) fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("ovs-db")
            .enable_all()
            .build()
            .expect("failed to start the database runtime")
    })
}

/// Runs `fut` on the database runtime and resolves with its output.
pub async fn run<F>(fut: F) -> F::Output
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    match runtime().spawn(fut).await {
        Ok(output) => output,
        Err(err) => std::panic::resume_unwind(err.into_panic()),
    }
}

pub fn driver_for(config: &ConnectionConfig) -> Arc<dyn Driver> {
    match config.engine {
        ovs_core::Engine::MySql => Arc::new(ovs_mysql::MySqlDriver),
        ovs_core::Engine::Sqlite => Arc::new(ovs_sqlite::SqliteDriver),
        ovs_core::Engine::Postgres => Arc::new(ovs_postgres::PostgresDriver),
    }
}

/// Opens a new session for `config`, through its SSH tunnel when it has one.
pub async fn connect(
    config: ConnectionConfig,
    password: Option<String>,
) -> ovs_core::Result<SharedSession> {
    run(async move {
        let mut config = config;
        if config.ssh.enabled {
            let port = tunnel_port(config.id).await?;
            // The driver talks to our end of the tunnel.
            "127.0.0.1".clone_into(&mut config.host);
            config.port = port;
        }
        let driver = driver_for(&config);
        let session = driver.connect(&config, password.as_deref()).await?;
        Ok(Arc::new(Mutex::new(session)))
    })
    .await
}

/// `~/` at the start of `path` means the home directory.
pub fn expand_home(path: &str) -> std::path::PathBuf {
    match path.strip_prefix("~/").zip(dirs::home_dir()) {
        Some((rest, home)) => home.join(rest),
        None => std::path::PathBuf::from(path),
    }
}

/// How to sign in to `config`'s SSH server, with `secret` as the password or key passphrase,
/// and the database's address as the SSH server sees it. `None` without SSH.
pub fn ssh_settings(
    config: &ConnectionConfig,
    secret: &str,
) -> Option<(SshSettings, (String, u16))> {
    let ssh = &config.ssh;
    if !ssh.enabled {
        return None;
    }
    let auth = match ssh.auth {
        SshAuthMethod::Password => SshAuth::Password(secret.to_owned()),
        SshAuthMethod::PrivateKey => SshAuth::PrivateKey {
            path: expand_home(&ssh.key_path),
            passphrase: Some(secret.to_owned()).filter(|s| !s.is_empty()),
        },
    };
    Some((
        SshSettings {
            host: ssh.host.clone(),
            port: ssh.port,
            user: ssh.user.clone(),
            auth,
        },
        (config.host.clone(), config.port),
    ))
}

/// How to reach a connection's database through SSH, kept for the life of the app so tunnels
/// can be reopened after a drop without asking for the secret again.
struct TunnelEntry {
    settings: SshSettings,
    target: (String, u16),
    known_fingerprint: Option<String>,
    tunnel: Option<Arc<Tunnel>>,
}

fn tunnels() -> &'static Mutex<HashMap<Uuid, TunnelEntry>> {
    static TUNNELS: OnceLock<Mutex<HashMap<Uuid, TunnelEntry>>> = OnceLock::new();
    TUNNELS.get_or_init(Mutex::default)
}

/// Records how to tunnel to connection `id`; the tunnel opens on the first `connect`.
/// `target` is the database host and port as seen from the SSH server.
pub async fn register_tunnel(
    id: Uuid,
    settings: SshSettings,
    target: (String, u16),
    known_fingerprint: Option<String>,
) {
    run(async move {
        let mut map = tunnels().lock().await;
        let reuse = map
            .get(&id)
            .filter(|e| e.settings == settings && e.target == target)
            .and_then(|e| e.tunnel.clone());
        map.insert(
            id,
            TunnelEntry {
                settings,
                target,
                known_fingerprint,
                tunnel: reuse,
            },
        );
    })
    .await;
}

/// The SSH server's host key fingerprint, once the tunnel for `id` has connected.
pub async fn tunnel_fingerprint(id: Uuid) -> Option<String> {
    run(async move {
        let map = tunnels().lock().await;
        let tunnel = map.get(&id)?.tunnel.as_ref()?;
        Some(tunnel.fingerprint().to_owned())
    })
    .await
}

/// The local port of connection `id`'s tunnel, opening (or reopening) it as needed.
async fn tunnel_port(id: Uuid) -> ovs_core::Result<u16> {
    // Held across the open so concurrent sessions share one tunnel.
    let mut map = tunnels().lock().await;
    let entry = map.get_mut(&id).ok_or_else(|| {
        ovs_core::Error::Connection(
            "SSH tunnel details are missing; reconnect from the connection screen".into(),
        )
    })?;
    if let Some(tunnel) = entry.tunnel.as_ref().filter(|t| t.is_alive()) {
        return Ok(tunnel.local_port());
    }
    let tunnel = Tunnel::open(
        &entry.settings,
        &entry.target.0,
        entry.target.1,
        entry.known_fingerprint.clone(),
    )
    .await
    .map_err(|e| ovs_core::Error::Connection(format!("SSH: {e}")))?;
    entry.known_fingerprint = Some(tunnel.fingerprint().to_owned());
    let port = tunnel.local_port();
    entry.tunnel = Some(Arc::new(tunnel));
    Ok(port)
}

#[cfg(test)]
pub(crate) mod tests {
    use ovs_core::{QueryOutcome, SshConfig, Value};

    use super::*;

    #[test]
    fn connects_to_sqlite_files() {
        let path = std::env::temp_dir().join(format!("ovs-app-{}.sqlite", std::process::id()));
        let config = ConnectionConfig {
            engine: ovs_core::Engine::Sqlite,
            path: path.to_string_lossy().into_owned(),
            ..ConnectionConfig::default()
        };
        let rows = runtime().block_on(async {
            let session = connect(config, None).await.unwrap();
            run(async move {
                let mut s = session.lock().await;
                s.execute("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (4)", None)
                    .await
                    .unwrap();
                s.execute("SELECT a FROM t", None).await.unwrap()
            })
            .await
        });
        let _ = std::fs::remove_file(&path);
        let QueryOutcome::Rows(rs) = &rows[0] else {
            panic!("{rows:?}")
        };
        assert_eq!(rs.rows[0][0], Value::Int(4));
    }

    /// The SSH server and database from `OVS_TEST_SSH`, which is
    /// `ssh_host:ssh_port:ssh_user:ssh_password:db_host:db_port` with the MySQL server as seen
    /// from the SSH server. Its root password is `OVS_TEST_DB_PASSWORD` or `secret`.
    pub(crate) fn ssh_test_config() -> Option<(ConnectionConfig, String)> {
        let spec = std::env::var("OVS_TEST_SSH").ok()?;
        let parts: Vec<&str> = spec.split(':').collect();
        let [ssh_host, ssh_port, ssh_user, ssh_password, db_host, db_port] = parts.as_slice()
        else {
            panic!("bad OVS_TEST_SSH");
        };
        let config = ConnectionConfig {
            host: (*db_host).to_owned(),
            port: db_port.parse().unwrap(),
            user: "root".to_owned(),
            ssh: SshConfig {
                enabled: true,
                host: (*ssh_host).to_owned(),
                port: ssh_port.parse().unwrap(),
                user: (*ssh_user).to_owned(),
                ..SshConfig::default()
            },
            ..ConnectionConfig::default()
        };
        Some((config, (*ssh_password).to_owned()))
    }

    /// Signs in to the SSH server the way a saved connection does, then opens two sessions
    /// through the one tunnel.
    fn query_through_tunnel(config: &ConnectionConfig, ssh_secret: &str, db_password: &str) {
        let (settings, target) = ssh_settings(config, ssh_secret).unwrap();
        runtime().block_on(async {
            register_tunnel(config.id, settings, target, None).await;
            for _ in 0..2 {
                let session = connect(config.clone(), Some(db_password.to_owned()))
                    .await
                    .unwrap();
                let out = session
                    .lock()
                    .await
                    .execute("SELECT 1 + 1", None)
                    .await
                    .unwrap();
                let QueryOutcome::Rows(rs) = &out[0] else {
                    panic!("expected rows")
                };
                assert_eq!(rs.rows[0][0], Value::Int(2));
            }
            let fingerprint = tunnel_fingerprint(config.id).await.unwrap();
            assert!(fingerprint.starts_with("SHA256:"));
        });
    }

    pub(crate) fn db_password() -> String {
        std::env::var("OVS_TEST_DB_PASSWORD").unwrap_or_else(|_| "secret".to_owned())
    }

    /// End to end through a real SSH server, signing in with a password.
    #[test]
    fn connects_through_an_ssh_tunnel() {
        let Some((config, ssh_password)) = ssh_test_config() else {
            return;
        };
        query_through_tunnel(&config, &ssh_password, &db_password());
    }

    /// Signing in with a private key instead: `OVS_TEST_SSH_KEY` names a key authorized for
    /// the same user, with passphrase `keypass`.
    #[test]
    fn connects_through_an_ssh_tunnel_with_a_key() {
        let (Some((mut config, _)), Ok(key)) =
            (ssh_test_config(), std::env::var("OVS_TEST_SSH_KEY"))
        else {
            return;
        };
        config.ssh.auth = SshAuthMethod::PrivateKey;
        config.ssh.key_path = key;
        query_through_tunnel(&config, "keypass", &db_password());

        // A wrong passphrase is reported, not retried as a password.
        config.id = Uuid::new_v4();
        let (settings, target) = ssh_settings(&config, "wrong").unwrap();
        let err = runtime().block_on(async {
            register_tunnel(config.id, settings, target, None).await;
            connect(config.clone(), Some(db_password())).await.err()
        });
        assert!(err.is_some_and(|e| e.to_string().contains("SSH")));
    }

    /// PostgreSQL through the same SSH server: `OVS_TEST_SSH_PG` is `db_host:db_port` as seen
    /// from the SSH server, for user `postgres`.
    #[test]
    fn connects_to_postgres_through_an_ssh_tunnel() {
        let (Some((mut config, ssh_password)), Ok(target)) =
            (ssh_test_config(), std::env::var("OVS_TEST_SSH_PG"))
        else {
            return;
        };
        let (host, port) = target.rsplit_once(':').expect("bad OVS_TEST_SSH_PG");
        config.engine = ovs_core::Engine::Postgres;
        host.clone_into(&mut config.host);
        config.port = port.parse().unwrap();
        "postgres".clone_into(&mut config.user);
        query_through_tunnel(&config, &ssh_password, &db_password());
    }
}
