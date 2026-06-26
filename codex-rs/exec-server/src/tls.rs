//! TLS support for the exec-server WebSocket transport.
//!
//! This module provides everything needed to run the exec-server over `wss://`
//! with a self-signed certificate and to connect a client that authenticates
//! the server by pinning the certificate's SHA-256 fingerprint. This achieves
//! end-to-end encryption of the JSON-RPC channel (commands, output, and file
//! contents) without requiring a reverse proxy to terminate TLS.
//!
//! Trust model:
//! - The server generates a self-signed certificate (see [`generate_self_signed_tls`])
//!   and prints its SHA-256 fingerprint at startup.
//! - The operator copies that fingerprint to the client (via the
//!   `CODEX_EXEC_SERVER_TLS_PINNED_SHA256` environment variable).
//! - The client uses [`FingerprintPinnedVerifier`] to accept *only* the
//!   certificate whose fingerprint matches, which prevents man-in-the-middle
//!   attacks even though the certificate is not signed by a public CA.

use std::sync::Arc;

use codex_utils_rustls_provider::ensure_rustls_crypto_provider;
use rcgen::CertifiedKey;
use rcgen::generate_simple_self_signed;
use rustls::ClientConfig;
use rustls::DigitallySignedStruct;
use rustls::Error as RustlsError;
use rustls::ServerConfig;
use rustls::SignatureScheme;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::client::danger::ServerCertVerified;
use rustls::client::danger::ServerCertVerifier;
use rustls::crypto::CryptoProvider;
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls_pki_types::CertificateDer;
use rustls_pki_types::PrivateKeyDer;
use rustls_pki_types::PrivatePkcs8KeyDer;
use rustls_pki_types::ServerName;
use rustls_pki_types::UnixTime;
use sha2::Digest;
use sha2::Sha256;

/// ALPN protocol negotiated for the WebSocket transport. WebSocket runs over an
/// HTTP/1.1 upgrade, so both ends advertise `http/1.1`.
const ALPN_HTTP_1_1: &[u8] = b"http/1.1";

/// Default subject alternative names baked into the generated certificate.
/// These cover the common local/loopback hosts; remote clients pin by
/// fingerprint and ignore the hostname, so the SAN list does not need to list
/// the public address.
const DEFAULT_SANS: &[&str] = &["localhost", "127.0.0.1", "::1"];

/// Errors produced while preparing TLS material.
#[derive(Debug, thiserror::Error)]
pub enum ExecServerTlsError {
    #[error("failed to generate self-signed certificate: {0}")]
    CertificateGeneration(#[from] rcgen::Error),
    #[error("failed to build rustls server config: {0}")]
    ServerConfig(#[source] RustlsError),
    #[error("invalid pinned certificate fingerprint: {0}")]
    InvalidFingerprint(String),
}

/// A freshly generated self-signed certificate plus the material needed to
/// serve it and to let clients pin it.
pub struct SelfSignedTls {
    pub cert_der: CertificateDer<'static>,
    pub key_der: PrivateKeyDer<'static>,
    /// SHA-256 over the DER-encoded certificate. This is the value clients pin
    /// and matches `openssl x509 -fingerprint -sha256` (minus the colons).
    pub fingerprint_sha256: [u8; 32],
}

impl SelfSignedTls {
    /// Returns the certificate fingerprint as lowercase hex (no separators).
    pub fn fingerprint_hex(&self) -> String {
        hex_encode(&self.fingerprint_sha256)
    }
}

/// Generates a self-signed certificate for the default loopback SANs.
pub fn generate_self_signed_tls() -> Result<SelfSignedTls, ExecServerTlsError> {
    let sans: Vec<String> = DEFAULT_SANS.iter().map(|s| (*s).to_string()).collect();
    let CertifiedKey { cert, signing_key } = generate_simple_self_signed(sans)?;

    let cert_der: CertificateDer<'static> = cert.der().clone();

    let mut hasher = Sha256::new();
    hasher.update(cert_der.as_ref());
    let fingerprint_sha256: [u8; 32] = hasher.finalize().into();

    let key_der: PrivateKeyDer<'static> =
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));

    Ok(SelfSignedTls {
        cert_der,
        key_der,
        fingerprint_sha256,
    })
}

/// Builds a rustls [`ServerConfig`] from a certificate/key pair, with no client
/// authentication and ALPN set to `http/1.1`.
pub fn build_server_config(
    cert_der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
) -> Result<Arc<ServerConfig>, ExecServerTlsError> {
    ensure_rustls_crypto_provider();
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .map_err(ExecServerTlsError::ServerConfig)?;
    config.alpn_protocols = vec![ALPN_HTTP_1_1.to_vec()];
    Ok(Arc::new(config))
}

/// A rustls [`ServerCertVerifier`] that accepts exactly one server certificate,
/// identified by its SHA-256 fingerprint. Hostname and CA-chain checks are
/// intentionally skipped because the certificate is self-signed; security comes
/// from the pinned fingerprint plus the standard TLS handshake signature
/// verification (delegated to the active crypto provider).
#[derive(Debug)]
pub struct FingerprintPinnedVerifier {
    expected_fingerprint: [u8; 32],
    supported_algs: WebPkiSupportedAlgorithms,
}

impl FingerprintPinnedVerifier {
    pub fn new(expected_fingerprint: [u8; 32]) -> Self {
        ensure_rustls_crypto_provider();
        let provider = CryptoProvider::get_default()
            .expect("rustls crypto provider must be installed before building a verifier");
        Self {
            expected_fingerprint,
            supported_algs: provider.signature_verification_algorithms,
        }
    }
}

impl ServerCertVerifier for FingerprintPinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        let mut hasher = Sha256::new();
        hasher.update(end_entity.as_ref());
        let actual: [u8; 32] = hasher.finalize().into();

        if constant_time_eq(&actual, &self.expected_fingerprint) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(RustlsError::General(
                "server certificate fingerprint does not match pinned value".to_string(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.supported_algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.supported_algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported_algs.supported_schemes()
    }
}

/// Builds a rustls [`ClientConfig`] that pins the server certificate by
/// fingerprint and advertises `http/1.1` via ALPN.
pub fn build_pinned_client_config(expected_fingerprint: [u8; 32]) -> Arc<ClientConfig> {
    ensure_rustls_crypto_provider();
    let verifier = Arc::new(FingerprintPinnedVerifier::new(expected_fingerprint));
    let mut config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    config.alpn_protocols = vec![ALPN_HTTP_1_1.to_vec()];
    Arc::new(config)
}

/// Parses a hex-encoded SHA-256 fingerprint (64 hex chars, optional `:`/space
/// separators, case-insensitive) into a 32-byte array.
pub fn parse_fingerprint_hex(value: &str) -> Result<[u8; 32], ExecServerTlsError> {
    let cleaned: String = value
        .chars()
        .filter(|c| !matches!(c, ':' | ' ' | '-'))
        .collect();
    if cleaned.len() != 64 {
        return Err(ExecServerTlsError::InvalidFingerprint(format!(
            "expected 64 hex characters, got {}",
            cleaned.len()
        )));
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = hex_val(cleaned.as_bytes()[i * 2])?;
        let lo = hex_val(cleaned.as_bytes()[i * 2 + 1])?;
        *byte = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_val(c: u8) -> Result<u8, ExecServerTlsError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        other => Err(ExecServerTlsError::InvalidFingerprint(format!(
            "invalid hex character: {:?}",
            other as char
        ))),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
#[path = "tls_tests.rs"]
mod tls_tests;
