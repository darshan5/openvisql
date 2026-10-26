//! Tests against a real SSH server. Skipped unless `OVS_TEST_SSH` is set to
//! `host:port:user:password:target_host:target_port`, where the target is a MySQL server
//! reachable from the SSH server. `OVS_TEST_SSH_KEY` may name a private key (passphrase
//! `keypass`) authorized for the same user. `OVS_TEST_SSH_PG` may name a PostgreSQL server
//! (`host:port`, as seen from the SSH server) to tunnel to as well.

use std::path::PathBuf;

use ovs_ssh::{SshAuth, SshError, SshSettings, Tunnel};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn config() -> Option<(SshSettings, String, u16)> {
    let spec = std::env::var("OVS_TEST_SSH").ok()?;
    let parts: Vec<&str> = spec.split(':').collect();
    let [host, port, user, password, target, target_port] = parts.as_slice() else {
        return None;
    };
    Some((
        SshSettings {
            host: (*host).to_owned(),
            port: port.parse().ok()?,
            user: (*user).to_owned(),
            auth: SshAuth::Password((*password).to_owned()),
        },
        (*target).to_owned(),
        target_port.parse().ok()?,
    ))
}

/// The first bytes MySQL sends: a handshake packet whose payload starts with protocol 10.
async fn reads_mysql_greeting(port: u16) {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut header = [0u8; 5];
    socket.read_exact(&mut header).await.unwrap();
    assert_eq!(header[4], 10, "not a MySQL handshake");
}

#[tokio::test]
async fn password_tunnel_reaches_the_database() {
    let Some((settings, target, port)) = config() else {
        return;
    };
    let tunnel = Tunnel::open(&settings, &target, port, None).await.unwrap();
    assert!(tunnel.fingerprint().starts_with("SHA256:"));
    assert!(tunnel.is_alive());
    // Two connections through one tunnel.
    reads_mysql_greeting(tunnel.local_port()).await;
    reads_mysql_greeting(tunnel.local_port()).await;
}

#[tokio::test]
async fn private_key_tunnel_works() {
    let Some((mut settings, target, port)) = config() else {
        return;
    };
    let Ok(key) = std::env::var("OVS_TEST_SSH_KEY") else {
        return;
    };
    settings.auth = SshAuth::PrivateKey {
        path: PathBuf::from(key),
        passphrase: Some("keypass".into()),
    };
    let tunnel = Tunnel::open(&settings, &target, port, None).await.unwrap();
    reads_mysql_greeting(tunnel.local_port()).await;
}

#[tokio::test]
async fn wrong_password_and_changed_host_key_are_refused() {
    let Some((mut settings, target, port)) = config() else {
        return;
    };
    let good = Tunnel::open(&settings, &target, port, None).await.unwrap();
    let seen = good.fingerprint().to_owned();
    // The remembered fingerprint is accepted.
    Tunnel::open(&settings, &target, port, Some(seen))
        .await
        .unwrap();

    let err = Tunnel::open(
        &settings,
        &target,
        port,
        Some("SHA256:not-this-server".into()),
    )
    .await
    .err()
    .unwrap();
    assert!(matches!(err, SshError::HostKeyChanged { .. }), "{err}");

    settings.auth = SshAuth::Password("wrong".into());
    let err = Tunnel::open(&settings, &target, port, None)
        .await
        .err()
        .unwrap();
    assert!(matches!(err, SshError::AuthFailed(_)), "{err}");
}

/// PostgreSQL answers an `SSLRequest` with a single `S` or `N`.
#[tokio::test]
async fn tunnel_reaches_postgres() {
    let (Some((settings, _, _)), Ok(pg)) = (config(), std::env::var("OVS_TEST_SSH_PG")) else {
        return;
    };
    let (host, port) = pg.rsplit_once(':').expect("bad OVS_TEST_SSH_PG");
    let tunnel = Tunnel::open(&settings, host, port.parse().unwrap(), None)
        .await
        .unwrap();
    let mut socket = TcpStream::connect(("127.0.0.1", tunnel.local_port()))
        .await
        .unwrap();
    socket
        .write_all(&[0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f])
        .await
        .unwrap();
    let mut answer = [0u8; 1];
    socket.read_exact(&mut answer).await.unwrap();
    assert!(matches!(&answer, b"S" | b"N"), "{answer:?}");
}
