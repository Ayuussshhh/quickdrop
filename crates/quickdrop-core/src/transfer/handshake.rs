//! Application-level handshake (over the established TLS stream).
//!
//! 1. Both sides send a [`Hello`] simultaneously.
//! 2. Both sides reply with [`Auth`], signing the *peer's* nonce
//!    with their Ed25519 secret key.
//! 3. Each side verifies the signature using the public key from
//!    the peer's HELLO and asserts the SHA-256 of that public key
//!    equals the advertised [`Fingerprint`].
//!
//! Anything that fails here aborts the connection before any
//! transfer-level data flows.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use rand::RngCore;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::identity::{DeviceIdentity, Fingerprint};
use crate::transfer::protocol::{Auth, Hello, AUTH_DOMAIN, PROTOCOL_VERSION};
use crate::transport;
use crate::{Error, Result};

/// Result of a successful handshake. Carries the peer's verified
/// public identity for subsequent authorisation decisions.
#[derive(Debug, Clone)]
pub struct PeerHandshake {
    pub hello: Hello,
}

pub async fn perform<S>(
    stream: &mut S,
    identity: &DeviceIdentity,
    device_name: String,
    os: crate::discovery::OsKind,
    device_type: crate::discovery::DeviceType,
) -> Result<PeerHandshake>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Build & send our HELLO.
    let mut nonce = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut nonce);
    let our_hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        id: identity.id(),
        name: device_name,
        os,
        device_type,
        fingerprint: identity.fingerprint(),
        verifying_key: identity.verifying_key_bytes(),
        nonce,
        app_version: crate::VERSION.to_string(),
    };
    transport::write_msg(stream, &our_hello).await?;

    // Read peer's HELLO.
    let peer_hello: Hello = transport::read_msg(stream).await?;
    if peer_hello.protocol_version != PROTOCOL_VERSION {
        return Err(Error::Protocol(format!(
            "protocol version mismatch: ours={} theirs={}",
            PROTOCOL_VERSION, peer_hello.protocol_version
        )));
    }

    // Verify advertised fingerprint matches the verifying key.
    let mut h = Sha256::new();
    h.update(peer_hello.verifying_key);
    let digest = h.finalize();
    if &digest[..16] != peer_hello.fingerprint.as_bytes() {
        return Err(Error::Protocol(
            "peer fingerprint does not match its public key".into(),
        ));
    }

    // Sign their nonce, send Auth.
    let mut to_sign = Vec::with_capacity(AUTH_DOMAIN.len() + peer_hello.nonce.len());
    to_sign.extend_from_slice(AUTH_DOMAIN);
    to_sign.extend_from_slice(&peer_hello.nonce);
    let sig = identity.sign(&to_sign);
    transport::write_msg(
        stream,
        &Auth {
            signature: sig.to_bytes().to_vec(),
        },
    )
    .await?;

    // Receive their Auth, verify.
    let peer_auth: Auth = transport::read_msg(stream).await?;
    if peer_auth.signature.len() != 64 {
        return Err(Error::Protocol("auth signature wrong length".into()));
    }
    let mut sig_arr = [0u8; 64];
    sig_arr.copy_from_slice(&peer_auth.signature);
    let signature = Signature::from_bytes(&sig_arr);

    let vk = VerifyingKey::from_bytes(&peer_hello.verifying_key)
        .map_err(|e| Error::Protocol(format!("peer key invalid: {e}")))?;
    let mut to_verify = Vec::with_capacity(AUTH_DOMAIN.len() + nonce.len());
    to_verify.extend_from_slice(AUTH_DOMAIN);
    to_verify.extend_from_slice(&nonce);
    vk.verify(&to_verify, &signature)
        .map_err(|e| Error::Protocol(format!("peer auth verify: {e}")))?;

    Ok(PeerHandshake { hello: peer_hello })
}

/// Convenience: extract the peer's fingerprint from a successful handshake.
pub fn peer_fingerprint(h: &PeerHandshake) -> Fingerprint {
    h.hello.fingerprint
}
