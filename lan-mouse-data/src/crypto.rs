//! Certificate identity for the data channel.
//!
//! The same PEM file is used by the DTLS input channel, so authorizing
//! a device once covers both connections. The helpers here convert the
//! PEM into rustls types and provide fingerprint pinning.

use std::{
    collections::HashSet,
    fs,
    io::{self, BufReader},
    path::Path,
    sync::{Arc, RwLock},
};

use rustls::{
    DigitallySignedStruct, DistinguishedName, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("failed to read certificate: {0}")]
    Io(#[from] io::Error),
    #[error("certificate file is not valid PEM: {0}")]
    Pem(#[from] pem::PemError),
    #[error("certificate file contains no certificate")]
    MissingCertificate,
    #[error("certificate file contains no private key")]
    MissingPrivateKey,
    #[error("unsupported private key format")]
    UnsupportedKey,
}

/// Local certificate and key shared with the DTLS input channel.
#[derive(Clone)]
pub struct Identity {
    pub certificate: CertificateDer<'static>,
    pub private_key: Vec<u8>,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("fingerprint", &certificate_fingerprint(&self.certificate))
            .finish_non_exhaustive()
    }
}

impl Identity {
    pub fn fingerprint(&self) -> String {
        certificate_fingerprint(&self.certificate)
    }
}

/// SHA-256 fingerprint in the `aa:bb:...` form used by lan-mouse.
pub fn certificate_fingerprint(cert: &[u8]) -> String {
    let digest = Sha256::digest(cert);
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Load the certificate and PKCS#8 key written by the DTLS channel.
pub fn load_identity(path: &Path) -> Result<Identity, IdentityError> {
    let file = fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut certificates = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| IdentityError::MissingCertificate)?;
    if certificates.is_empty() {
        return Err(IdentityError::MissingCertificate);
    }
    // The first certificate is the leaf used on the wire; later entries
    // would be a chain, which self-signed lan-mouse certificates do not use.
    let certificate = certificates.remove(0);

    // lan-mouse writes the PKCS#8 key with a non-standard
    // `PRIVATE_KEY` tag (webrtc-dtls' parser requires it), which
    // `rustls_pemfile` silently skips. Parse the PEM blocks here and
    // accept both that tag and the standard `PRIVATE KEY`.
    let mut key_der = None;
    for block in pem::parse_many(fs::read(path)?)? {
        if matches!(block.tag(), "PRIVATE_KEY" | "PRIVATE KEY") {
            key_der = Some(block.into_contents());
            break;
        }
    }
    let private_key = key_der.ok_or(IdentityError::MissingPrivateKey)?;

    Ok(Identity {
        certificate,
        private_key,
    })
}

/// Set of SHA-256 certificate fingerprints allowed to use the data and
/// input channels. Shared so that authorizing or revoking a device takes
/// effect without restarting the listeners.
pub type AuthorizedFingerprints = Arc<RwLock<HashSet<String>>>;

/// Rejects any certificate whose fingerprint is not explicitly allowed.
///
/// Normal WebPKI chain verification cannot be used because lan-mouse
/// authorizes self-signed device certificates by fingerprint. The
/// handshake signature is still verified against the presented key, so
/// an attacker who does not hold the matching private key cannot
/// complete the handshake even if a fingerprint were guessed.
#[derive(Debug, Clone)]
pub struct PinnedVerifier {
    allowed: AuthorizedFingerprints,
    provider: Arc<CryptoProvider>,
    required: bool,
    pairing_only: bool,
}

impl PinnedVerifier {
    pub fn new(
        allowed: AuthorizedFingerprints,
        provider: Arc<CryptoProvider>,
        required: bool,
    ) -> Self {
        Self {
            allowed,
            provider,
            required,
            pairing_only: false,
        }
    }

    /// Only for the restricted enrollment socket. This proves possession of
    /// the presented key; neither certificate is trusted until users confirm.
    pub(crate) fn pairing(provider: Arc<CryptoProvider>) -> Self {
        Self {
            allowed: Default::default(),
            provider,
            required: true,
            pairing_only: true,
        }
    }

    fn check(&self, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        if self.pairing_only {
            return Ok(());
        }
        let fingerprint = certificate_fingerprint(cert);
        let allowed = self
            .allowed
            .read()
            .map_err(|_| rustls::Error::General("authorized fingerprint set is poisoned".into()))?;
        if allowed.contains(&fingerprint) {
            return Ok(());
        }
        log::warn!("rejected unauthorized data-channel certificate {fingerprint}");
        Err(rustls::Error::InvalidCertificate(
            rustls::CertificateError::UnknownIssuer,
        ))
    }

    fn supported_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
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
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported_schemes()
    }
}

impl ClientCertVerifier for PinnedVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        // Every device presents a self-signed certificate, so there is no
        // useful CA subject to advertise. An empty list tells the client
        // to send its configured certificate.
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
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
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported_schemes()
    }

    fn client_auth_mandatory(&self) -> bool {
        self.required
    }

    fn offer_client_auth(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn fingerprint_is_sha256_hex_colon_separated() {
        let fingerprint = certificate_fingerprint(b"hello");
        assert_eq!(fingerprint.len(), 32 * 3 - 1);
        assert!(
            fingerprint
                .split(':')
                .all(|part| part.len() == 2 && part.chars().all(|c| c.is_ascii_hexdigit()))
        );
    }

    #[test]
    fn loads_lan_mouse_pem_with_custom_key_tag() {
        let dir = std::env::temp_dir().join(format!("lan-mouse-data-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lan-mouse.pem");

        let params = rcgen::CertificateParams::new(vec!["test".into()]).unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let pem = format!(
            "-----BEGIN PRIVATE_KEY-----\n{}\n-----END PRIVATE_KEY-----\n-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            base64_encode(&key.serialize_der()),
            base64_encode(cert.der())
        );
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(pem.as_bytes()).unwrap();

        let identity = load_identity(&path).unwrap();
        assert!(!identity.private_key.is_empty());
        assert_eq!(
            identity.fingerprint(),
            certificate_fingerprint(identity.certificate.as_ref())
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    fn base64_encode(bytes: &[u8]) -> String {
        // Tiny test-only encoder to avoid adding a base64 dependency.
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            out.push(ALPHABET[(n >> 18) as usize & 63] as char);
            out.push(ALPHABET[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                ALPHABET[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }
}
