//! TLS transport with public-key pinning and length-prefixed
//! MessagePack framing.
//!
//! ## TLS
//!
//! Each device generates a self-signed X.509 certificate at runtime
//! (via `rcgen`). The cert's Subject Public Key Info contains an
//! ed25519-derived material — but for *pinning* purposes we hash the
//! cert's DER and the certificate is simply transported. Both peers
//! exchange a signed Ed25519 HELLO over the encrypted channel, which
//! is what actually authenticates the device. The TLS layer's only
//! job is to provide confidentiality + integrity for the bytes; the
//! identity check is done in `protocol::handshake`.
//!
//! Both client and server therefore use a custom verifier that
//! accepts **any** certificate (we don't trust a CA — we trust the
//! Ed25519 signature in the HELLO). This is intentional; rolling our
//! own CA chain on every device would just move the problem.
//!
//! ## Framing
//!
//! Every control message is a 4-byte big-endian length followed by a
//! MessagePack-encoded value. Bulk file data uses the same framing
//! but with a typed `Frame::FileChunk { len, .. }` so the reader can
//! stream the body straight to disk without buffering the whole
//! chunk.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, ServerConfig, SignatureScheme};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::{Error, Result};

/// Maximum size of a single control message (everything except file
/// chunks). 16 MiB is huge headroom for manifests; protects us from
/// a malicious or buggy peer trying to OOM the receiver.
pub const MAX_CONTROL_FRAME: usize = 16 * 1024 * 1024;

/// Pre-shared ALPN identifier so a stray browser hitting our port
/// fails immediately rather than going through a full handshake.
pub const ALPN: &[u8] = b"quickdrop/1";

/// Generates a fresh self-signed cert + key in DER form. Cheap
/// (~ms) — we do this once at process startup.
pub fn generate_self_signed() -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let cert = rcgen::generate_simple_self_signed(vec!["quickdrop.local".to_string()])
        .map_err(|e| Error::Transport(format!("rcgen: {e}")))?;
    let cert_der = CertificateDer::from(cert.cert.der().to_vec());
    let key_der = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    Ok((cert_der, PrivateKeyDer::Pkcs8(key_der)))
}

/// Build a server-side TLS config that accepts any client cert. Real
/// authentication happens in the application-level handshake.
pub fn server_config(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> Result<Arc<ServerConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Transport(format!("server tls: {e}")))?
        .with_client_cert_verifier(Arc::new(AcceptAnyClientCert(provider)))
        .with_single_cert(vec![cert], key)
        .map_err(|e| Error::Transport(format!("server cert: {e}")))?;
    let mut cfg = cfg;
    cfg.alpn_protocols = vec![ALPN.to_vec()];
    Ok(Arc::new(cfg))
}

/// Build a client-side TLS config that accepts any server cert.
pub fn client_config() -> Result<Arc<ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Transport(format!("client tls: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert(provider)))
        .with_no_client_auth();
    let mut cfg = cfg;
    cfg.alpn_protocols = vec![ALPN.to_vec()];
    Ok(Arc::new(cfg))
}

pub fn acceptor(cfg: Arc<ServerConfig>) -> TlsAcceptor {
    TlsAcceptor::from(cfg)
}

pub fn connector(cfg: Arc<ClientConfig>) -> TlsConnector {
    TlsConnector::from(cfg)
}

/// SNI hostname used by the client. Servers ignore the value; we
/// pick a fixed string so the TLS spec is satisfied.
pub fn sni() -> ServerName<'static> {
    ServerName::try_from("quickdrop.local").expect("static sni")
}

// ---------------------------------------------------------------------
// Custom cert verifiers
// ---------------------------------------------------------------------

#[derive(Debug)]
struct AcceptAnyServerCert(Arc<rustls::crypto::CryptoProvider>);

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
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[derive(Debug)]
struct AcceptAnyClientCert(Arc<rustls::crypto::CryptoProvider>);

impl ClientCertVerifier for AcceptAnyClientCert {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> std::result::Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
    fn client_auth_mandatory(&self) -> bool {
        false
    }
    fn offer_client_auth(&self) -> bool {
        false
    }
}

// Suppress unused: the verifier needs to live but is only consulted
// by rustls' machinery, never directly by us.
#[allow(dead_code)]
fn _root_store() -> RootCertStore {
    RootCertStore::empty()
}

// ---------------------------------------------------------------------
// Length-prefixed MessagePack framing
// ---------------------------------------------------------------------

/// Read a 4-byte BE length prefix, then exactly `len` bytes, decoding
/// as MessagePack.
pub async fn read_msg<R, T>(r: &mut R) -> Result<T>
where
    R: AsyncReadExt + Unpin,
    T: for<'de> serde::Deserialize<'de>,
{
    let len = read_len(r).await?;
    if len > MAX_CONTROL_FRAME {
        return Err(Error::Protocol(format!(
            "control frame too large: {len} bytes"
        )));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)
        .await
        .map_err(|e| Error::Transport(format!("read body: {e}")))?;
    rmp_serde::from_slice(&buf).map_err(Error::from)
}

pub async fn write_msg<W, T>(w: &mut W, msg: &T) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
    T: serde::Serialize,
{
    let body = rmp_serde::to_vec_named(msg)?;
    if body.len() > MAX_CONTROL_FRAME {
        return Err(Error::Protocol(format!(
            "control frame too large: {} bytes",
            body.len()
        )));
    }
    let len = (body.len() as u32).to_be_bytes();
    w.write_all(&len)
        .await
        .map_err(|e| Error::Transport(format!("write len: {e}")))?;
    w.write_all(&body)
        .await
        .map_err(|e| Error::Transport(format!("write body: {e}")))?;
    w.flush()
        .await
        .map_err(|e| Error::Transport(format!("flush: {e}")))?;
    Ok(())
}

/// Read just the length prefix. Used by streaming readers that want
/// to forward the body straight to disk.
pub async fn read_len<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<usize> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)
        .await
        .map_err(|e| Error::Transport(format!("read len: {e}")))?;
    Ok(u32::from_be_bytes(len_buf) as usize)
}

pub async fn write_len<W: AsyncWriteExt + Unpin>(w: &mut W, len: u32) -> Result<()> {
    w.write_all(&len.to_be_bytes())
        .await
        .map_err(|e| Error::Transport(format!("write len: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use tokio::io::duplex;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Sample {
        a: u32,
        b: String,
    }

    #[tokio::test]
    async fn frame_roundtrip() {
        let (mut a, mut b) = duplex(1024);
        let m = Sample {
            a: 42,
            b: "hello".into(),
        };
        write_msg(&mut a, &m).await.unwrap();
        let got: Sample = read_msg(&mut b).await.unwrap();
        assert_eq!(got, m);
    }

    #[tokio::test]
    async fn rejects_oversized_frame() {
        let (mut a, mut b) = duplex(8);
        let too_big = ((MAX_CONTROL_FRAME + 1) as u32).to_be_bytes();
        a.write_all(&too_big).await.unwrap();
        let r: Result<Sample> = read_msg(&mut b).await;
        assert!(matches!(r, Err(Error::Protocol(_))));
    }

    #[test]
    fn self_signed_cert_generates() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (_cert, _key) = generate_self_signed().expect("self-signed");
    }
}
