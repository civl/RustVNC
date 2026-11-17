//! TLS material for talking to the agent.
//!
//! Two distinct modes, and the difference matters:
//!
//! * **Pairing** has nothing to verify yet, so the server certificate is
//!   accepted provisionally. The caller must compare the fingerprint the agent
//!   reports out of band (menu bar) with the one pinned afterwards — and this
//!   module additionally checks that the *TLS peer certificate* hashes to the
//!   fingerprint the agent claimed, so a man in the middle cannot hand us one
//!   certificate and claim another.
//! * **Normal sessions** pin the CA that was stored at pairing time and present
//!   the client certificate the agent issued.

use std::io::BufReader;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, RootCertStore, SignatureScheme};
use sha2::{Digest, Sha256};

use crate::protocol;

pub fn install_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Pairing only: the server is not authenticated yet.
///
/// The fingerprint of whatever certificate the server presents is surfaced
/// through [`fingerprint_der`] on the peer certificate, and the caller refuses
/// to pin a CA whose server certificate does not match the fingerprint the
/// agent announced.
#[derive(Debug)]
struct AcceptAnyServerCert {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for AcceptAnyServerCert {
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
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

pub fn pairing_client_config() -> Result<quinn::ClientConfig> {
    install_provider();

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut crypto =
        rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert { provider }))
            .with_no_client_auth();
    crypto.alpn_protocols = vec![protocol::ALPN.to_vec()];

    let quic = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
        .context("convert rustls client config to QUIC")?;
    Ok(quinn::ClientConfig::new(Arc::new(quic)))
}

pub fn authenticated_client_config(
    ca_cert_pem: &str,
    client_cert_pem: &str,
    client_key_pem: &str,
) -> Result<quinn::ClientConfig> {
    install_provider();

    let ca_certs = certs_from_pem(ca_cert_pem)?;
    let mut roots = RootCertStore::empty();
    for cert in &ca_certs {
        roots.add(cert.clone()).context("add CA root")?;
    }

    let mut chain = certs_from_pem(client_cert_pem)?;
    chain.extend(ca_certs);
    let key = key_from_pem(client_key_pem)?;

    let mut crypto =
        rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
            .with_root_certificates(roots)
            .with_client_auth_cert(chain, key)
            .context("configure client certificate")?;
    crypto.alpn_protocols = vec![protocol::ALPN.to_vec()];

    let quic = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
        .context("convert rustls client config to QUIC")?;
    Ok(quinn::ClientConfig::new(Arc::new(quic)))
}

pub fn fingerprint_der(cert: &CertificateDer<'_>) -> String {
    hex::encode(Sha256::digest(cert.as_ref()))
}

/// Fingerprint of the certificate chain the QUIC peer actually presented.
pub fn peer_fingerprint(connection: &quinn::Connection) -> Option<String> {
    let identity = connection.peer_identity()?;
    let chain = identity.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    chain.first().map(fingerprint_der)
}

pub fn certs_from_pem(pem: &str) -> Result<Vec<CertificateDer<'static>>> {
    let mut reader = BufReader::new(pem.as_bytes());
    rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parse PEM certificates")
}

fn key_from_pem(pem: &str) -> Result<PrivateKeyDer<'static>> {
    let mut reader = BufReader::new(pem.as_bytes());
    rustls_pemfile::private_key(&mut reader)
        .context("parse PEM private key")?
        .ok_or_else(|| anyhow!("PEM contains no private key"))
}

/// Parse `host:port`, tolerating IPv6 literals in brackets.
pub fn split_addr(addr: &str) -> Result<(String, u16)> {
    if let Some(rest) = addr.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| anyhow!("unterminated IPv6 literal in {addr}"))?;
        let port = tail
            .strip_prefix(':')
            .ok_or_else(|| anyhow!("missing port in {addr}"))?
            .parse()
            .with_context(|| format!("parse port in {addr}"))?;
        return Ok((host.to_string(), port));
    }
    let (host, port) = addr
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("expected host:port, got {addr}"))?;
    if host.is_empty() {
        bail!("empty host in {addr}");
    }
    Ok((
        host.to_string(),
        port.parse()
            .with_context(|| format!("parse port in {addr}"))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_ipv4_and_ipv6_addresses() {
        assert_eq!(
            split_addr("192.168.1.20:8443").unwrap(),
            ("192.168.1.20".to_string(), 8443)
        );
        assert_eq!(split_addr("[::1]:8443").unwrap(), ("::1".to_string(), 8443));
        assert!(split_addr("no-port").is_err());
        assert!(split_addr("[::1]8443").is_err());
    }
}
