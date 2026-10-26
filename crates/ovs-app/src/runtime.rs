//! Bridges GPUI's executor and tokio. Database drivers need a tokio reactor, while views run
//! on GPUI's own executor, so every driver call is spawned onto a shared tokio runtime and its
//! `JoinHandle` (an ordinary future) is awaited from GPUI tasks.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, OnceLock};

use ovs_core::{ConnectionConfig, Driver, Session};
use ovs_ssh::{SshSettings, Tunnel};
use tokio::runtime::Runtime;
use tokio::sync::Mutex;
use uuid::Uuid;

/// A session shared between the view that owns it and the tokio tasks that use it.
pub type SharedSession = Arc<Mutex<Box<dyn Session>>>;

fn runtime() -> &'static Runtime {
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
mod tests {
    use ovs_core::{QueryOutcome, SshConfig, Value};
    use ovs_ssh::SshAuth;

    use super::*;

    /// End to end through a real SSH server. Skipped unless `OVS_TEST_SSH` is
    /// `ssh_host:ssh_port:ssh_user:ssh_password:db_host:db_port` (the database as seen from the
    /// SSH server); the database user is root with password `OVS_TEST_DB_PASSWORD` or `secret`.
    #[test]
    fn connects_through_an_ssh_tunnel() {
        let Ok(spec) = std::env::var("OVS_TEST_SSH") else {
            return;
        };
        let parts: Vec<&str> = spec.split(':').collect();
        let [ssh_host, ssh_port, ssh_user, ssh_password, db_host, db_port] = parts.as_slice()
        else {
            panic!("bad OVS_TEST_SSH");
        };
        let db_password =
            std::env::var("OVS_TEST_DB_PASSWORD").unwrap_or_else(|_| "secret".to_owned());
        let config = ConnectionConfig {
            host: (*db_host).to_owned(),
            port: db_port.parse().unwrap(),
            ssh: SshConfig {
                enabled: true,
                host: (*ssh_host).to_owned(),
                port: ssh_port.parse().unwrap(),
                user: (*ssh_user).to_owned(),
                ..SshConfig::default()
            },
            ..ConnectionConfig::default()
        };
        let settings = SshSettings {
            host: config.ssh.host.clone(),
            port: config.ssh.port,
            user: config.ssh.user.clone(),
            auth: SshAuth::Password((*ssh_password).to_owned()),
        };
        runtime().block_on(async {
            register_tunnel(
                config.id,
                settings,
                (config.host.clone(), config.port),
                None,
            )
            .await;
            // Two sessions share one tunnel.
            for _ in 0..2 {
                let session = connect(config.clone(), Some(db_password.clone()))
                    .await
                    .unwrap();
                let out = session
                    .lock()
                    .await
                    .execute("SELECT 1", None)
                    .await
                    .unwrap();
                let QueryOutcome::Rows(rs) = &out[0] else {
                    panic!("expected rows")
                };
                assert_eq!(rs.rows[0][0], Value::Int(1));
            }
            let fingerprint = tunnel_fingerprint(config.id).await.unwrap();
            assert!(fingerprint.starts_with("SHA256:"));
        });
    }
}
