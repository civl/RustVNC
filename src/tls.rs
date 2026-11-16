use std::fs;
use std::io::BufReader;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DnType,
    ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, RootCertStore, SignatureScheme};
use sha2::{Digest, Sha256};

use crate::paths;
use crate::protocol;
use crate::secrets::SecretStore;

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
    pub fingerprint: String,
}

pub fn install_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn ensure_material(dir: &Path, extra_sans: &[String], keychain: bool) -> Result<Material> {
    paths::ensure_private_dir(dir).context("create agent state dir")?;

    let store = SecretStore::agent(dir, keychain);
    let ca_cert_path = dir.join("ca-cert.pem");
    let server_cert_path = dir.join("server-cert.pem");

    let ca_cert = read_optional_string(&ca_cert_path)?;
    let ca_key = store.get("ca-key", "ca-key.pem")?;
    let server_cert = read_optional_string(&server_cert_path)?;
    let server_key = store.get("server-key", "server-key.pem")?;

    let (ca_cert_pem, ca_key_pem, server_cert_pem, server_key_pem) =
        match (ca_cert, ca_key, server_cert, server_key) {
            (None, None, None, None) => generate_all(dir, &store, extra_sans)?,
            (Some(ca_cert), Some(ca_key), Some(server_cert), Some(server_key)) => (
                ca_cert,
                String::from_utf8(ca_key).context("CA key is not UTF-8 PEM")?,
                server_cert,
                String::from_utf8(server_key).context("server key is not UTF-8 PEM")?,
            ),
            (Some(ca_cert), Some(ca_key), _, _) => {
                let ca_key_pem = String::from_utf8(ca_key).context("CA key is not UTF-8 PEM")?;
                let (server_cert_pem, server_key_pem) =
                    regenerate_server(dir, &store, &ca_cert, &ca_key_pem, extra_sans)?;
                (ca_cert, ca_key_pem, server_cert_pem, server_key_pem)
            }
            (Some(_), None, _, _) => {
                bail!("CA certificate exists but CA private key is missing")
            }
            (None, Some(_), _, _) => {
                bail!("CA private key exists but CA certificate is missing")
            }
            _ => bail!("inconsistent certificate state in {}", dir.display()),
        };

    // Migrate keys from legacy 0600 files into Keychain on first use.
    if keychain {
        if dir.join("ca-key.pem").exists() {
            let _ = store.set("ca-key", "ca-key.pem", ca_key_pem.as_bytes());
        }
        if dir.join("server-key.pem").exists() {
            let _ = store.set("server-key", "server-key.pem", server_key_pem.as_bytes());
        }
    }

    let fingerprint = fingerprint_pem(&server_cert_pem)?;
    Ok(Material {
        ca_cert_pem,
        ca_key_pem,
        server_cert_pem,
        server_key_pem,
        server_fingerprint: fingerprint,
    })
}

fn generate_all(
    dir: &Path,
    store: &SecretStore,
    extra_sans: &[String],
) -> Result<(String, String, String, String)> {
    let ca_key = KeyPair::generate().context("generate CA key")?;
    let ca_params = ca_params()?;
    let ca_cert = ca_params.self_signed(&ca_key).context("self-sign CA")?;

    let ca_cert_pem = ca_cert.pem();
    let ca_key_pem = ca_key.serialize_pem();
    store.set("ca-key", "ca-key.pem", ca_key_pem.as_bytes())?;
    write_cert(&dir.join("ca-cert.pem"), &ca_cert_pem)?;

    let (server_cert_pem, server_key_pem) =
        sign_server(dir, store, &ca_cert_pem, &ca_key_pem, extra_sans)?;

    Ok((ca_cert_pem, ca_key_pem, server_cert_pem, server_key_pem))
}

fn regenerate_server(
    dir: &Path,
    store: &SecretStore,
    ca_cert_pem: &str,
    ca_key_pem: &str,
    extra_sans: &[String],
) -> Result<(String, String)> {
    sign_server(dir, store, ca_cert_pem, ca_key_pem, extra_sans)
}

fn sign_server(
    dir: &Path,
    store: &SecretStore,
    ca_cert_pem: &str,
    ca_key_pem: &str,
    extra_sans: &[String],
) -> Result<(String, String)> {
    let issuer = Issuer::from_ca_cert_pem(ca_cert_pem, KeyPair::from_pem(ca_key_pem)?)
        .context("load CA issuer")?;

    let server_key = KeyPair::generate().context("generate server key")?;
    let server_params = server_params(extra_sans)?;
    let server_cert = server_params
        .signed_by(&server_key, &issuer)
        .context("sign server certificate")?;

    let server_cert_pem = server_cert.pem();
    let server_key_pem = server_key.serialize_pem();
    store.set("server-key", "server-key.pem", server_key_pem.as_bytes())?;
    write_cert(&dir.join("server-cert.pem"), &server_cert_pem)?;
    Ok((server_cert_pem, server_key_pem))
}

fn ca_params() -> Result<CertificateParams> {
    let mut params = CertificateParams::new(Vec::<String>::new()).context("CA params")?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::CommonName, "RustVNC Agent CA");
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    Ok(params)
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

pub fn issue_client_cert(
    material: &Material,
    client_name: &str,
    csr_pem: &str,
) -> Result<IssuedClient> {
    let csr = CertificateSigningRequestParams::from_pem(csr_pem).context("parse CSR")?;
    if matches!(csr.params.is_ca, IsCa::Ca(_)) {
        bail!("CSR must not request a CA certificate");
    }

    let mut params = csr.params.clone();
    params.distinguished_name = Default::default();
    params
        .distinguished_name
        .push(DnType::CommonName, sanitize_client_name(client_name));
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    params.use_authority_key_identifier_extension = true;

    let issuer = Issuer::from_ca_cert_pem(
        &material.ca_cert_pem,
        KeyPair::from_pem(&material.ca_key_pem).context("load CA key")?,
    )
    .context("load CA issuer")?;

    let cert = params
        .signed_by(&csr.public_key, &issuer)
        .context("sign client certificate")?;

    Ok(IssuedClient {
        cert_pem: cert.pem(),
        fingerprint: fingerprint_der(cert.der()),
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

fn read_optional_string(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
    }
}

fn write_cert(path: &Path, pem: &str) -> Result<()> {
    fs::write(path, pem).with_context(|| format!("write {}", path.display()))?;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    Ok(())
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
