//! Tests against a real SSH server. Skipped unless `OVS_TEST_SSH` is set to
//! `host:port:user:password:target_host:target_port`, where the target is a MySQL server
//! reachable from the SSH server. `OVS_TEST_SSH_KEY` may name a private key (passphrase
//! `keypass`) authorized for the same user.

use std::path::PathBuf;

use ovs_ssh::{SshAuth, SshError, SshSettings, Tunnel};
use tokio::io::AsyncReadExt;
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
