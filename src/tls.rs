use std::fs;
use std::io::BufReader;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, RootCertStore, SignatureScheme};
use sha2::{Digest, Sha256};

use crate::paths;
use crate::protocol;

#[derive(Clone, Debug)]
pub struct Material {
    pub ca_cert_pem: String,
    pub ca_key_pem: String,
    pub server_cert_pem: String,
    pub server_key_pem: String,
    pub server_fingerprint: String,
}

#[derive(Clone, Debug)]
pub struct IssuedClient {
    pub cert_pem: String,
    pub key_pem: String,
    pub fingerprint: String,
}

pub fn install_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn ensure_material(dir: &Path, extra_sans: &[String]) -> Result<Material> {
    paths::ensure_private_dir(dir).context("create agent state dir")?;

    let ca_cert_path = dir.join("ca-cert.pem");
    let ca_key_path = dir.join("ca-key.pem");
    let server_cert_path = dir.join("server-cert.pem");
    let server_key_path = dir.join("server-key.pem");

    let all_exist = ca_cert_path.exists()
        && ca_key_path.exists()
        && server_cert_path.exists()
        && server_key_path.exists();

    if !all_exist {
        generate_material(
            &ca_cert_path,
            &ca_key_path,
            &server_cert_path,
            &server_key_path,
            extra_sans,
        )?;
    }

    // Keep permissions tight even for files created by an older build.
    for path in [&ca_key_path, &server_key_path] {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }

    let material = Material {
        ca_cert_pem: fs::read_to_string(&ca_cert_path).context("read CA certificate")?,
        ca_key_pem: fs::read_to_string(&ca_key_path).context("read CA key")?,
        server_cert_pem: fs::read_to_string(&server_cert_path)
            .context("read server certificate")?,
        server_key_pem: fs::read_to_string(&server_key_path).context("read server key")?,
        server_fingerprint: String::new(),
    };

    let fingerprint = fingerprint_pem(&material.server_cert_pem)?;
    Ok(Material {
        server_fingerprint: fingerprint,
        ..material
    })
}

fn generate_material(
    ca_cert_path: &Path,
    ca_key_path: &Path,
    server_cert_path: &Path,
    server_key_path: &Path,
    extra_sans: &[String],
) -> Result<()> {
    let ca_key = KeyPair::generate().context("generate CA key")?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).context("CA params")?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "RustVNC Agent CA");
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca_cert = ca_params.self_signed(&ca_key).context("self-sign CA")?;

    let ca_cert_pem = ca_cert.pem();
    let ca_key_pem = ca_key.serialize_pem();
    paths::write_private(ca_cert_path, ca_cert_pem.as_bytes())?;
    paths::write_private(ca_key_path, ca_key_pem.as_bytes())?;

    let issuer = Issuer::from_ca_cert_pem(&ca_cert_pem, KeyPair::from_pem(&ca_key_pem)?)
        .context("load generated CA issuer")?;

    let server_key = KeyPair::generate().context("generate server key")?;
    let server_params = server_params(extra_sans)?;
    let server_cert = server_params
        .signed_by(&server_key, &issuer)
        .context("sign server certificate")?;

    paths::write_private(server_cert_path, server_cert.pem().as_bytes())?;
    paths::write_private(server_key_path, server_key.serialize_pem().as_bytes())?;
    Ok(())
}

fn server_params(extra_sans: &[String]) -> Result<CertificateParams> {
    let mut sans = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    for san in extra_sans {
        if !sans.iter().any(|existing| existing == san) {
            sans.push(san.clone());
        }
    }

    let mut params = CertificateParams::new(sans).context("server certificate params")?;
    params
        .distinguished_name
        .push(DnType::CommonName, "RustVNC Agent");
    params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    params
        .extended_key_usages
        .push(ExtendedKeyUsagePurpose::ServerAuth);
    params.use_authority_key_identifier_extension = true;
    Ok(params)
}

pub fn issue_client_cert(material: &Material, client_name: &str) -> Result<IssuedClient> {
    let issuer = Issuer::from_ca_cert_pem(
        &material.ca_cert_pem,
        KeyPair::from_pem(&material.ca_key_pem).context("load CA key")?,
    )
    .context("load CA issuer")?;

    let key = KeyPair::generate().context("generate client key")?;
    let mut params =
        CertificateParams::new(Vec::<String>::new()).context("client certificate params")?;
    params
        .distinguished_name
        .push(DnType::CommonName, sanitize_client_name(client_name));
    params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    params
        .extended_key_usages
        .push(ExtendedKeyUsagePurpose::ClientAuth);
    params.use_authority_key_identifier_extension = true;

    let cert = params
        .signed_by(&key, &issuer)
        .context("sign client certificate")?;
    let fingerprint = fingerprint_der(cert.der());

    Ok(IssuedClient {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
        fingerprint,
    })
}

fn sanitize_client_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        "RustVNC Client".to_string()
    } else {
        trimmed.chars().take(64).collect()
    }
}

pub fn server_config(material: &Material) -> Result<quinn::ServerConfig> {
    install_provider();

    let ca_certs = certs_from_pem(&material.ca_cert_pem)?;
    let mut roots = RootCertStore::empty();
    for cert in &ca_certs {
        roots
            .add(cert.clone())
            .context("add CA to client-verifier roots")?;
    }

    let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
        .allow_unauthenticated()
        .build()
        .context("build client certificate verifier")?;

    let mut chain = certs_from_pem(&material.server_cert_pem)?;
    chain.extend(ca_certs);
    let key = key_from_pem(&material.server_key_pem)?;

    let mut crypto =
        rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
            .with_client_cert_verifier(verifier)
            .with_single_cert(chain, key)
            .context("configure server certificate")?;
    crypto.alpn_protocols = vec![protocol::ALPN.to_vec()];

    let quic = quinn::crypto::rustls::QuicServerConfig::try_from(crypto)
        .context("convert rustls server config to QUIC")?;
    Ok(quinn::ServerConfig::with_crypto(Arc::new(quic)))
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

pub fn fingerprint_pem(pem: &str) -> Result<String> {
    let certs = certs_from_pem(pem)?;
    let cert = certs
        .first()
        .ok_or_else(|| anyhow!("empty certificate chain"))?;
    Ok(fingerprint_der(cert))
}

pub fn fingerprint_der(cert: &CertificateDer<'_>) -> String {
    hex::encode(Sha256::digest(cert.as_ref()))
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

/// Pairing only: the server is not authenticated yet. The client compares the
/// server fingerprint out-of-band (menu bar / terminal) and pins the CA after
/// pairing. Never use this for normal connections.
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
