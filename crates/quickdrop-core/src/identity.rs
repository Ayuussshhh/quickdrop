//! Local device identity.
//!
//! On first run, QuickDrop generates an **Ed25519** keypair and stores
//! the 32-byte secret seed in the operating system's secure credential
//! store (Windows Credential Manager on Windows, Keychain on macOS,
//! Secret Service on Linux) via the `keyring` crate. The public key is
//! derived on every load; nothing identity-related is ever persisted to
//! disk in cleartext.
//!
//! The device's **UUID** and 16-byte **fingerprint** are derived
//! deterministically from the public key:
//!
//! ```text
//! H = SHA-256(public_key_bytes)
//! fingerprint = H[0..16]
//! device_id   = UUID::from_bytes(H[16..32])
//! ```
//!
//! This means identity survives a wiped sled database as long as the
//! keyring entry is intact, and it is impossible for two devices with
//! different public keys to collide on either field unless SHA-256 is
//! broken.
//!
//! Loading is split through the [`KeyStore`] trait so unit tests can
//! exercise the full create/load logic against an in-memory store
//! without writing to the user's real credential manager.

use std::fmt;
use std::sync::Mutex;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey, SECRET_KEY_LENGTH};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{Error, Result};

/// Service name used in the OS credential store.
const KEYRING_SERVICE: &str = "QuickDrop";
/// Account name. Bumping this constant (e.g. `v2`) forces a clean
/// regeneration of the device identity, leaving any previous keypair
/// orphaned in the credential store. We don't expect to need this, but
/// it is a deliberate escape hatch for emergency key rotation.
const KEYRING_ACCOUNT: &str = "device-identity-v1";

/// Abstraction over the secure store that holds the device's private
/// key seed. Production code uses [`KeyringStore`]; tests use
/// [`MemoryKeyStore`].
pub trait KeyStore: Send + Sync {
    /// Returns the stored seed (32 bytes hex-encoded), or `None` if
    /// nothing has been stored yet.
    fn get(&self) -> Result<Option<String>>;
    fn set(&self, encoded: &str) -> Result<()>;
    fn delete(&self) -> Result<()>;
}

/// Production [`KeyStore`] backed by the OS credential manager.
#[derive(Debug)]
pub struct KeyringStore {
    service: &'static str,
    account: &'static str,
}

impl KeyringStore {
    pub fn new() -> Self {
        Self {
            service: KEYRING_SERVICE,
            account: KEYRING_ACCOUNT,
        }
    }

    fn entry(&self) -> Result<keyring::Entry> {
        keyring::Entry::new(self.service, self.account)
            .map_err(|e| Error::Internal(format!("keyring entry init: {e}")))
    }
}

impl Default for KeyringStore {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyStore for KeyringStore {
    fn get(&self) -> Result<Option<String>> {
        match self.entry()?.get_password() {
            Ok(s) => Ok(Some(s)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(Error::Internal(format!("keyring get_password: {e}"))),
        }
    }

    fn set(&self, encoded: &str) -> Result<()> {
        self.entry()?
            .set_password(encoded)
            .map_err(|e| Error::Internal(format!("keyring set_password: {e}")))
    }

    fn delete(&self) -> Result<()> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(Error::Internal(format!("keyring delete: {e}"))),
        }
    }
}

/// In-memory [`KeyStore`] for tests. Thread-safe.
#[derive(Debug, Default)]
pub struct MemoryKeyStore {
    inner: Mutex<Option<String>>,
}

impl KeyStore for MemoryKeyStore {
    fn get(&self) -> Result<Option<String>> {
        Ok(self.inner.lock().unwrap().clone())
    }
    fn set(&self, encoded: &str) -> Result<()> {
        *self.inner.lock().unwrap() = Some(encoded.to_string());
        Ok(())
    }
    fn delete(&self) -> Result<()> {
        *self.inner.lock().unwrap() = None;
        Ok(())
    }
}

/// 16-byte truncated SHA-256 of the device's Ed25519 public key.
///
/// Used for two purposes:
/// 1. **Certificate pinning** when establishing TLS to a peer — the
///    self-signed cert's public-key SHA-256 must match.
/// 2. **Visual verification** during first-time pairing (rendered as
///    eight groups of four hex chars).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Fingerprint(pub [u8; 16]);

impl Fingerprint {
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Renders the fingerprint as `aabb-ccdd-eeff-...` (16 hex chars
    /// per pair, separated by dashes). Easier to read aloud than a
    /// solid hex string.
    pub fn display_grouped(&self) -> String {
        let hex_str = hex::encode(self.0);
        // 32 hex chars total → 8 groups of 4
        let mut out = String::with_capacity(32 + 7);
        for (i, chunk) in hex_str.as_bytes().chunks(4).enumerate() {
            if i > 0 {
                out.push('-');
            }
            out.push_str(std::str::from_utf8(chunk).expect("ascii hex"));
        }
        out
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({})", hex::encode(self.0))
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display_grouped())
    }
}

/// Public, serialisable view of a device's identity.
///
/// This is what we put on the wire (mDNS TXT, HELLO message, etc.).
/// Note that it deliberately does **not** include the secret key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicIdentity {
    pub id: Uuid,
    pub fingerprint: Fingerprint,
    /// Raw 32-byte Ed25519 verifying key.
    pub verifying_key: [u8; 32],
}

/// The local device's full identity. Holds the secret key in memory
/// (zeroized on drop by `ed25519-dalek`'s `SigningKey`).
pub struct DeviceIdentity {
    id: Uuid,
    fingerprint: Fingerprint,
    signing_key: SigningKey,
}

impl fmt::Debug for DeviceIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Deliberately do not print the secret key.
        f.debug_struct("DeviceIdentity")
            .field("id", &self.id)
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl DeviceIdentity {
    /// Convenience: load or create using the production [`KeyringStore`].
    pub fn load_or_create() -> Result<Self> {
        Self::load_or_create_with(&KeyringStore::new())
    }

    /// Loads the device's keypair from `store`, or generates a new one
    /// and persists it on first run.
    ///
    /// Errors only when both reading and writing the store fail —
    /// i.e. the OS service is genuinely unavailable. In that case the
    /// caller should surface a clear message rather than silently
    /// regenerating a key on every launch.
    pub fn load_or_create_with(store: &dyn KeyStore) -> Result<Self> {
        let signing_key = match store.get()? {
            Some(stored) => decode_signing_key(&stored)?,
            None => {
                tracing::info!("no existing identity — generating new Ed25519 keypair");
                let key = SigningKey::generate(&mut OsRng);
                let encoded = hex::encode(key.to_bytes());
                store.set(&encoded)?;
                key
            }
        };
        Ok(Self::from_signing_key(signing_key))
    }

    /// Builds a [`DeviceIdentity`] from an already-decoded signing key.
    /// Used by the loaders and tests.
    fn from_signing_key(signing_key: SigningKey) -> Self {
        let pub_bytes = signing_key.verifying_key().to_bytes();
        let hash = Sha256::digest(pub_bytes);
        let mut fp = [0u8; 16];
        fp.copy_from_slice(&hash[..16]);
        let mut id_bytes = [0u8; 16];
        id_bytes.copy_from_slice(&hash[16..32]);
        Self {
            id: Uuid::from_bytes(id_bytes),
            fingerprint: Fingerprint(fp),
            signing_key,
        }
    }

    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    pub fn verifying_key_bytes(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    /// Public, serialisable view safe to ship over the network.
    pub fn public(&self) -> PublicIdentity {
        PublicIdentity {
            id: self.id,
            fingerprint: self.fingerprint,
            verifying_key: self.verifying_key_bytes(),
        }
    }

    pub fn sign(&self, msg: &[u8]) -> Signature {
        self.signing_key.sign(msg)
    }

    /// **Test / repair only.** Wipes the stored identity from the
    /// production credential store. The next call to [`load_or_create`]
    /// will generate a fresh keypair, which means every previously
    /// paired peer will treat this device as untrusted on next contact.
    /// Hide this behind a "Reset device identity" button in settings.
    pub fn wipe_stored() -> Result<()> {
        KeyringStore::new().delete()
    }
}

fn decode_signing_key(stored: &str) -> Result<SigningKey> {
    let bytes = hex::decode(stored.trim())
        .map_err(|_| Error::Internal("identity: stored key is not valid hex".into()))?;
    if bytes.len() != SECRET_KEY_LENGTH {
        return Err(Error::Internal(format!(
            "identity: expected {SECRET_KEY_LENGTH}-byte secret, got {} bytes",
            bytes.len()
        )));
    }
    let mut arr = [0u8; SECRET_KEY_LENGTH];
    arr.copy_from_slice(&bytes);
    Ok(SigningKey::from_bytes(&arr))
}

/// Verify a signature produced by some peer's [`PublicIdentity`].
pub fn verify(public: &PublicIdentity, msg: &[u8], sig: &Signature) -> Result<()> {
    let vk = VerifyingKey::from_bytes(&public.verifying_key)
        .map_err(|e| Error::Protocol(format!("invalid verifying key: {e}")))?;
    vk.verify(msg, sig)
        .map_err(|e| Error::Protocol(format!("signature verification failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_or_create_is_idempotent() {
        let store = MemoryKeyStore::default();
        let a = DeviceIdentity::load_or_create_with(&store).expect("first load");
        let b = DeviceIdentity::load_or_create_with(&store).expect("second load");
        assert_eq!(a.id(), b.id(), "id must be stable across loads");
        assert_eq!(
            a.fingerprint(),
            b.fingerprint(),
            "fingerprint must be stable"
        );
        assert_eq!(a.verifying_key_bytes(), b.verifying_key_bytes());
    }

    #[test]
    fn fresh_store_yields_new_identity() {
        let s1 = MemoryKeyStore::default();
        let s2 = MemoryKeyStore::default();
        let a = DeviceIdentity::load_or_create_with(&s1).unwrap();
        let b = DeviceIdentity::load_or_create_with(&s2).unwrap();
        assert_ne!(a.id(), b.id());
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn wiping_store_regenerates_identity() {
        let store = MemoryKeyStore::default();
        let a = DeviceIdentity::load_or_create_with(&store).unwrap();
        store.delete().unwrap();
        let b = DeviceIdentity::load_or_create_with(&store).unwrap();
        assert_ne!(a.id(), b.id());
    }

    #[test]
    fn fingerprint_and_id_are_pubkey_derived() {
        let key = SigningKey::generate(&mut OsRng);
        let pub_bytes = key.verifying_key().to_bytes();
        let id = DeviceIdentity::from_signing_key(key);
        let h = Sha256::digest(pub_bytes);
        assert_eq!(id.fingerprint().as_bytes(), &h[..16]);
        assert_eq!(id.id().as_bytes(), &h[16..32]);
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let key = SigningKey::generate(&mut OsRng);
        let id = DeviceIdentity::from_signing_key(key);
        let msg = b"hello quickdrop";
        let sig = id.sign(msg);
        verify(&id.public(), msg, &sig).expect("signature should verify");
    }

    #[test]
    fn fingerprint_display_grouped_format() {
        let fp = Fingerprint([
            0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99,
        ]);
        assert_eq!(fp.display_grouped(), "aabb-ccdd-eeff-0011-2233-4455-6677-8899");
    }

    #[test]
    fn corrupt_stored_key_is_rejected() {
        let store = MemoryKeyStore::default();
        store.set("not-hex").unwrap();
        let err = DeviceIdentity::load_or_create_with(&store).unwrap_err();
        match err {
            Error::Internal(msg) => assert!(msg.contains("not valid hex")),
            other => panic!("unexpected error: {other:?}"),
        }
    }
}

