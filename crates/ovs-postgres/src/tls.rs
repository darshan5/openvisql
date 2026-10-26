//! TLS for PostgreSQL connections, with rustls (the `ring` backend, as for MySQL).
//!
//! "Encrypt only" accepts any server certificate. "Verify certificate" checks it against the
//! web's root authorities plus an optional CA file, and checks the host name.

use std::sync::Arc;

use ovs_core::{ConnectionConfig, Error, Result, SslMode};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio_postgres_rustls::MakeRustlsConnect;

/// Checks the handshake's signatures but trusts any certificate: encryption without
/// verification.
#[derive(Debug)]
struct AnyCertificate(WebPkiSupportedAlgorithms);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}

/// The TLS connector for `config`, or `None` when SSL is off.
pub(crate) fn connector(config: &ConnectionConfig) -> Result<Option<MakeRustlsConnect>> {
    let err = |e: rustls::Error| Error::Connection(format!("TLS setup failed: {e}"));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(err)?;
    let tls = match config.ssl {
        SslMode::Disabled => return Ok(None),
        SslMode::Preferred => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCertificate(
                provider.signature_verification_algorithms,
            )))
            .with_no_client_auth(),
        SslMode::Required => {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            if let Some(ca) = config.ssl_ca.as_deref().filter(|p| !p.trim().is_empty()) {
                let certs = CertificateDer::pem_file_iter(ca.trim())
                    .map_err(|e| Error::Connection(format!("CA file {ca}: {e}")))?;
                for cert in certs {
                    let cert = cert.map_err(|e| Error::Connection(format!("CA file {ca}: {e}")))?;
                    roots.add(cert).map_err(err)?;
                }
            }
            builder.with_root_certificates(roots).with_no_client_auth()
        }
    };
    Ok(Some(MakeRustlsConnect::new(tls)))
}
