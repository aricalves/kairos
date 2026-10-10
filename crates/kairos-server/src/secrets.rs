//! The encryption of the secrets that Kairos keeps in the database
//! (COLLIERY-T-3105): today, the read token of a repository.
//!
//! AES-256-GCM (`ring`), with the key of the deployment setting
//! `KAIROS_SECRETS_KEY` (32 bytes, base64). Each encryption has a new random
//! nonce of 12 bytes. The caller gives associated data that names the row
//! (for a token: the tenant slug and the repository id), so a ciphertext
//! that is copied to a different row does not decrypt.
//!
//! Each row also keeps the [`SecretsKey::id`] of the key that encrypted it: a
//! short fingerprint, not the key. When the deployment has a different key
//! now, [`SecretsKey::open`] says so ([`OpenError::OtherKey`]), and the
//! operator knows to set the secret again. A decryption that fails with the
//! same key is [`OpenError::Corrupt`]: the row was changed or moved.
//!
//! The key can be rotated (KAIROS-T-0370). A [`SecretsKey`] is a keyring:
//! the key of `KAIROS_SECRETS_KEY` encrypts, and the old keys of
//! `KAIROS_SECRETS_KEY_OLD` (a list, separated by commas) only decrypt. The
//! command `kairos-server rotate-secrets` ([`crate::secrets_rotation`])
//! encrypts each sealed row again with the new key. After it, the operator
//! removes `KAIROS_SECRETS_KEY_OLD`.

use base64::Engine as _;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// The name of the setting, for the texts that name it.
pub const SECRETS_KEY_VAR: &str = "KAIROS_SECRETS_KEY";

/// The name of the setting of the old keys (KAIROS-T-0370): a list of keys
/// in base64, separated by commas. They decrypt; they never encrypt.
pub const SECRETS_KEY_OLD_VAR: &str = "KAIROS_SECRETS_KEY_OLD";

/// The keyring of `KAIROS_SECRETS_KEY` and `KAIROS_SECRETS_KEY_OLD`. The
/// first key encrypts; each key decrypts the rows that it encrypted. Its
/// `Debug` shows only the fingerprints.
#[derive(Clone)]
pub struct SecretsKey {
    key: [u8; 32],
    id: String,
    /// The old keys: `(key, fingerprint)`.
    old: Vec<([u8; 32], String)>,
}

impl std::fmt::Debug for SecretsKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let old: Vec<&str> = self.old.iter().map(|(_, id)| id.as_str()).collect();
        f.debug_struct("SecretsKey")
            .field("id", &self.id)
            .field("old", &old)
            .finish()
    }
}

/// The fingerprint of a key of 32 bytes.
fn fingerprint(key: &[u8; 32]) -> String {
    let digest = Sha256::new()
        .chain_update(b"kairos-secrets-key-id\0")
        .chain_update(key)
        .finalize();
    digest[..6].iter().map(|b| format!("{b:02x}")).collect()
}

/// 32 bytes from base64. The error says what is wrong, with no part of the
/// value.
fn decode_key(raw: &str) -> Result<[u8; 32], String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(raw.trim())
        .map_err(|_| "it is not base64. Make a key with `openssl rand -base64 32`".to_string())?;
    bytes.try_into().map_err(|bytes: Vec<u8>| {
        format!(
            "it has {} bytes, and it must have 32 bytes. Make a key with `openssl rand \
             -base64 32`",
            bytes.len()
        )
    })
}

/// A secret after [`SecretsKey::seal`].
#[derive(Clone, PartialEq, Eq)]
pub struct Sealed {
    /// The ciphertext and the tag of 16 bytes.
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
    /// The [`SecretsKey::id`] of the key.
    pub key_id: String,
}

/// Why [`SecretsKey::open`] gave no secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OpenError {
    /// A different key encrypted the secret.
    #[error("The token was set with a different KAIROS_SECRETS_KEY. Set the token again.")]
    OtherKey,
    /// The key is the same, but the ciphertext does not decrypt: the row was
    /// changed, or it is the row of a different repository.
    #[error("The stored token does not decrypt. Set the token again.")]
    Corrupt,
}

impl SecretsKey {
    /// The key from its base64 text. The error says what is wrong, with no
    /// part of the value.
    pub fn from_base64(raw: &str) -> Result<Self, String> {
        Ok(Self::from_bytes(decode_key(raw)?))
    }

    /// The key from its 32 bytes.
    pub fn from_bytes(key: [u8; 32]) -> Self {
        let id = fingerprint(&key);
        Self {
            key,
            id,
            old: Vec::new(),
        }
    }

    /// Add the old keys of `KAIROS_SECRETS_KEY_OLD`: base64 keys separated
    /// by commas. An empty entry is skipped; the key that encrypts is not
    /// added twice. The error names the position of the bad entry, with no
    /// part of the value.
    pub fn with_old_base64(mut self, raw: &str) -> Result<Self, String> {
        for (index, entry) in raw.split(',').map(str::trim).enumerate() {
            if entry.is_empty() {
                continue;
            }
            let key = decode_key(entry).map_err(|e| format!("entry {}: {e}", index + 1))?;
            self = self.with_old_bytes(key);
        }
        Ok(self)
    }

    /// Add one old key.
    pub fn with_old_bytes(mut self, key: [u8; 32]) -> Self {
        let id = fingerprint(&key);
        if id != self.id && !self.old.iter().any(|(_, old)| *old == id) {
            self.old.push((key, id));
        }
        self
    }

    /// The fingerprints of the old keys.
    pub fn old_ids(&self) -> Vec<&str> {
        self.old.iter().map(|(_, id)| id.as_str()).collect()
    }

    /// Did a key other than the key that encrypts seal this row? Then
    /// `rotate-secrets` seals it again.
    pub fn needs_reseal(&self, key_id: &str) -> bool {
        key_id != self.id
    }

    /// The fingerprint of the key: 12 hex characters. It does not tell the
    /// key.
    pub fn id(&self) -> &str {
        &self.id
    }

    fn cipher(key: &[u8; 32]) -> LessSafeKey {
        // A key of 32 bytes is always a correct AES-256 key.
        LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).expect("a 32-byte AES key"))
    }

    /// Encrypt `secret`, bound to `aad`.
    pub fn seal(&self, aad: &[u8], secret: &[u8]) -> Sealed {
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .expect("the system random source works");
        let mut in_out = secret.to_vec();
        Self::cipher(&self.key)
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut in_out,
            )
            .expect("AES-GCM encrypts a short secret");
        Sealed {
            ciphertext: in_out,
            nonce: nonce.to_vec(),
            key_id: self.id.clone(),
        }
    }

    /// Decrypt a secret that [`Self::seal`] encrypted with `aad`.
    pub fn open(
        &self,
        aad: &[u8],
        key_id: &str,
        nonce: &[u8],
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, OpenError> {
        let key = if key_id == self.id {
            &self.key
        } else {
            match self.old.iter().find(|(_, id)| id == key_id) {
                Some((key, _)) => key,
                None => return Err(OpenError::OtherKey),
            }
        };
        let nonce: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| OpenError::Corrupt)?;
        let mut in_out = ciphertext.to_vec();
        let plain = Self::cipher(key)
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut in_out,
            )
            .map_err(|_| OpenError::Corrupt)?;
        Ok(plain.to_vec())
    }
}

/// The associated data of the read token of a repository: the tenant and
/// the repository, so that the ciphertext decrypts only in its own row.
pub fn credential_aad(tenant: &str, repository_id: Uuid) -> Vec<u8> {
    format!("kairos/repository-credential/v1\0{tenant}\0{repository_id}").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> SecretsKey {
        SecretsKey::from_bytes([byte; 32])
    }

    #[test]
    fn a_sealed_secret_opens_with_its_key_and_its_row() {
        let k = key(7);
        let repo = Uuid::new_v4();
        let aad = credential_aad("acme", repo);
        let sealed = k.seal(&aad, b"github_pat_secret");
        assert_eq!(sealed.key_id, k.id());
        assert_eq!(sealed.nonce.len(), 12);
        assert!(
            !sealed
                .ciphertext
                .windows(b"github_pat_secret".len())
                .any(|w| w == b"github_pat_secret"),
            "the ciphertext must not hold the secret"
        );
        let opened = k
            .open(&aad, &sealed.key_id, &sealed.nonce, &sealed.ciphertext)
            .expect("opens");
        assert_eq!(opened, b"github_pat_secret");
        // A new nonce for each seal.
        assert_ne!(k.seal(&aad, b"github_pat_secret").nonce, sealed.nonce);
    }

    #[test]
    fn a_secret_of_a_different_row_or_tenant_does_not_open() {
        let k = key(7);
        let repo = Uuid::new_v4();
        let sealed = k.seal(&credential_aad("acme", repo), b"tok");
        let other_repo = credential_aad("acme", Uuid::new_v4());
        let other_tenant = credential_aad("globex", repo);
        for aad in [other_repo, other_tenant] {
            assert_eq!(
                k.open(&aad, &sealed.key_id, &sealed.nonce, &sealed.ciphertext),
                Err(OpenError::Corrupt)
            );
        }
    }

    #[test]
    fn a_changed_key_says_set_it_again() {
        let repo = Uuid::new_v4();
        let aad = credential_aad("acme", repo);
        let sealed = key(7).seal(&aad, b"tok");
        let err = key(8)
            .open(&aad, &sealed.key_id, &sealed.nonce, &sealed.ciphertext)
            .unwrap_err();
        assert_eq!(err, OpenError::OtherKey);
        assert!(err.to_string().contains("Set the token again"), "{err}");
        assert!(err.to_string().contains("KAIROS_SECRETS_KEY"), "{err}");
    }

    /// KAIROS-T-0370: an old key decrypts; the new key encrypts.
    #[test]
    fn an_old_key_of_the_keyring_opens_and_the_new_key_seals() {
        let repo = Uuid::new_v4();
        let aad = credential_aad("acme", repo);
        let sealed = key(7).seal(&aad, b"tok");
        let ring = key(8).with_old_bytes([7u8; 32]);
        assert_eq!(ring.old_ids(), vec![key(7).id()]);
        assert!(ring.needs_reseal(&sealed.key_id));
        let opened = ring
            .open(&aad, &sealed.key_id, &sealed.nonce, &sealed.ciphertext)
            .expect("an old key opens");
        assert_eq!(opened, b"tok");
        let again = ring.seal(&aad, &opened);
        assert_eq!(again.key_id, key(8).id());
        assert!(!ring.needs_reseal(&again.key_id));
        // A key that is not in the ring is still OtherKey.
        let stray = key(9).seal(&aad, b"tok");
        assert_eq!(
            ring.open(&aad, &stray.key_id, &stray.nonce, &stray.ciphertext),
            Err(OpenError::OtherKey)
        );
    }

    #[test]
    fn the_old_keys_are_a_list_of_base64() {
        let b64 = |b: u8| base64::engine::general_purpose::STANDARD.encode([b; 32]);
        let ring = key(8)
            .with_old_base64(&format!("{}, ,{},{}", b64(7), b64(6), b64(8)))
            .expect("a good list");
        // The key that encrypts is not added as an old key.
        assert_eq!(ring.old_ids(), vec![key(7).id(), key(6).id()]);
        let err = key(8)
            .with_old_base64(&format!("{},short", b64(7)))
            .unwrap_err();
        assert!(err.starts_with("entry 2:"), "{err}");
        assert!(!format!("{ring:?}").contains("[7, 7"), "{ring:?}");
    }

    #[test]
    fn the_key_is_32_bytes_of_base64() {
        let good = base64::engine::general_purpose::STANDARD.encode([1u8; 32]);
        let k = SecretsKey::from_base64(&format!(" {good}\n")).expect("good key");
        assert_eq!(k.id().len(), 12);
        assert_eq!(k.id(), SecretsKey::from_bytes([1u8; 32]).id());
        assert_ne!(k.id(), key(2).id());
        let short = base64::engine::general_purpose::STANDARD.encode([1u8; 16]);
        let err = SecretsKey::from_base64(&short).unwrap_err();
        assert!(err.contains("16 bytes"), "{err}");
        let err = SecretsKey::from_base64("not base64!").unwrap_err();
        assert!(err.contains("not base64"), "{err}");
        // The debug text has no key bytes.
        assert!(!format!("{k:?}").contains("[1, 1"), "{k:?}");
    }
}
