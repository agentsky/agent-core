//! Encryption at rest: [`Sealer`].
//!
//! Every encrypted column holds `version(1) || nonce(12) || ciphertext`, where
//! the ciphertext is ChaCha20-Poly1305 output with its 16-byte tag at the end.
//! The associated data is `table/column/primary key`, so a value copied into
//! another row or column fails to decrypt instead of being read as that row's
//! secret.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chacha20poly1305::aead::{Generate, Nonce, Tag};
use chacha20poly1305::{AeadInOut, ChaCha20Poly1305, KeyInit};
use secrecy::zeroize::Zeroizing;
use secrecy::{ExposeSecret, SecretString};

/// The version byte written in front of every sealed value. A key rotation
/// adds a new version so values sealed under the old key can still be read
/// and re-sealed.
const KEY_VERSION: u8 = 1;
/// The length of a master key in bytes.
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const HEADER_LEN: usize = 1 + NONCE_LEN;

/// The error returned when a master key can't be loaded.
///
/// It never repeats the key, or any part of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    /// The key is not standard base64.
    #[error("master key is not valid base64")]
    Base64,
    /// The key does not decode to 32 bytes.
    #[error("master key must decode to {KEY_LEN} bytes")]
    Length,
}

/// The error returned when a value can't be sealed or opened.
///
/// It never contains the plaintext or the ciphertext.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SealError {
    /// The stored value is shorter than a version byte, a nonce and a tag.
    #[error("sealed value is truncated")]
    Truncated,
    /// The stored value was sealed under a key version this build doesn't
    /// have.
    #[error("sealed value has unknown key version {0}")]
    UnknownVersion(u8),
    /// Authentication failed: the key is wrong, the value was altered, or it
    /// was sealed for another row or column.
    #[error("sealed value failed to authenticate")]
    Decrypt,
    /// The value decrypted, but is not UTF-8 text.
    #[error("sealed value is not UTF-8")]
    NotUtf8,
    /// The value is too long for ChaCha20-Poly1305.
    #[error("value is too long to seal")]
    TooLong,
    /// The operating system's random number generator failed.
    #[error("random number generator failed")]
    Rng,
}

/// Where a sealed value lives: its table, column and row's primary key.
///
/// It is the associated data of the value, written as `table/column/key`.
/// Table and column names never contain `/`, so the prefix is unambiguous
/// and no two locations share associated data.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Aad<'a> {
    pub(crate) table: &'static str,
    pub(crate) column: &'static str,
    pub(crate) key: &'a str,
}

impl Aad<'_> {
    fn to_bytes(self) -> Vec<u8> {
        format!("{}/{}/{}", self.table, self.column, self.key).into_bytes()
    }
}

/// Seals and opens secret column values with ChaCha20-Poly1305.
///
/// Built from the 32-byte master key (`AGENTD_MASTER_KEY`, standard base64).
/// Each value gets a fresh random 96-bit nonce. `Debug` shows nothing of the
/// key.
pub struct Sealer {
    cipher: ChaCha20Poly1305,
}

impl fmt::Debug for Sealer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sealer").finish_non_exhaustive()
    }
}

impl Sealer {
    /// Loads the master key from its standard base64 form. Whitespace around
    /// it, such as a trailing newline, is ignored.
    ///
    /// # Errors
    ///
    /// [`KeyError`] if the key isn't base64 or isn't 32 bytes long.
    pub fn from_base64(key: &SecretString) -> Result<Self, KeyError> {
        let bytes = Zeroizing::new(
            STANDARD
                .decode(key.expose_secret().trim())
                .map_err(|_| KeyError::Base64)?,
        );
        if bytes.len() != KEY_LEN {
            return Err(KeyError::Length);
        }
        let cipher = ChaCha20Poly1305::new_from_slice(&bytes).map_err(|_| KeyError::Length)?;
        Ok(Self { cipher })
    }

    /// Generates a new random master key in the form
    /// [`from_base64`](Self::from_base64) reads.
    ///
    /// # Errors
    ///
    /// [`SealError::Rng`] if the operating system's random number generator
    /// fails.
    pub fn generate_key() -> Result<SecretString, SealError> {
        let key = Zeroizing::new(<[u8; KEY_LEN]>::try_generate().map_err(|_| SealError::Rng)?);
        Ok(SecretString::from(STANDARD.encode(key.as_slice())))
    }

    /// Encrypts `plaintext` for the location `aad`.
    pub(crate) fn seal(
        &self,
        aad: Aad<'_>,
        plaintext: &SecretString,
    ) -> Result<Vec<u8>, SealError> {
        let plaintext = plaintext.expose_secret().as_bytes();
        let nonce = Nonce::<ChaCha20Poly1305>::try_generate().map_err(|_| SealError::Rng)?;
        let mut out = Vec::with_capacity(HEADER_LEN + plaintext.len() + TAG_LEN);
        out.push(KEY_VERSION);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(plaintext);
        let tag = self
            .cipher
            .encrypt_inout_detached(&nonce, &aad.to_bytes(), (&mut out[HEADER_LEN..]).into())
            .map_err(|_| SealError::TooLong)?;
        out.extend_from_slice(&tag);
        Ok(out)
    }

    /// Decrypts a value sealed for the location `aad`.
    pub(crate) fn open(&self, aad: Aad<'_>, sealed: &[u8]) -> Result<SecretString, SealError> {
        let (&version, rest) = sealed.split_first().ok_or(SealError::Truncated)?;
        if rest.len() < NONCE_LEN + TAG_LEN {
            return Err(SealError::Truncated);
        }
        if version != KEY_VERSION {
            return Err(SealError::UnknownVersion(version));
        }
        let (nonce, rest) = rest.split_at(NONCE_LEN);
        let (ciphertext, tag) = rest.split_at(rest.len() - TAG_LEN);
        let nonce = Nonce::<ChaCha20Poly1305>::try_from(nonce).map_err(|_| SealError::Truncated)?;
        let tag = Tag::<ChaCha20Poly1305>::try_from(tag).map_err(|_| SealError::Truncated)?;
        let mut buf = Zeroizing::new(ciphertext.to_vec());
        self.cipher
            .decrypt_inout_detached(&nonce, &aad.to_bytes(), buf.as_mut_slice().into(), &tag)
            .map_err(|_| SealError::Decrypt)?;
        let text = std::str::from_utf8(&buf).map_err(|_| SealError::NotUtf8)?;
        Ok(SecretString::from(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sealer() -> Sealer {
        Sealer::from_base64(&Sealer::generate_key().unwrap()).unwrap()
    }

    const AAD: Aad<'static> = Aad {
        table: "claude_links",
        column: "access_token_enc",
        key: "67e55044-10b1-426f-9247-bb680e5fe0c8",
    };

    #[test]
    fn round_trip() {
        let sealer = sealer();
        let sealed = sealer.seal(AAD, &SecretString::from("sk-ant-oat")).unwrap();
        assert_eq!(
            sealer.open(AAD, &sealed).unwrap().expose_secret(),
            "sk-ant-oat"
        );
        let empty = sealer.seal(AAD, &SecretString::from("")).unwrap();
        assert_eq!(sealer.open(AAD, &empty).unwrap().expose_secret(), "");
    }

    #[test]
    fn layout_is_version_nonce_ciphertext_tag() {
        let sealer = sealer();
        let sealed = sealer.seal(AAD, &SecretString::from("secret")).unwrap();
        assert_eq!(sealed.len(), 1 + 12 + "secret".len() + 16);
        assert_eq!(sealed[0], KEY_VERSION);
        assert!(!sealed.windows(6).any(|w| w == b"secret"));
    }

    #[test]
    fn each_value_gets_a_fresh_nonce() {
        let sealer = sealer();
        let plaintext = SecretString::from("same");
        let a = sealer.seal(AAD, &plaintext).unwrap();
        let b = sealer.seal(AAD, &plaintext).unwrap();
        assert_ne!(a[1..HEADER_LEN], b[1..HEADER_LEN]);
        assert_ne!(a, b);
    }

    #[test]
    fn a_value_moved_to_another_row_or_column_fails() {
        let sealer = sealer();
        let sealed = sealer.seal(AAD, &SecretString::from("secret")).unwrap();
        let other_row = Aad {
            key: "00000000-0000-4000-8000-000000000000",
            ..AAD
        };
        let other_column = Aad {
            column: "refresh_token_enc",
            ..AAD
        };
        let other_table = Aad {
            table: "pending_logins",
            ..AAD
        };
        for aad in [other_row, other_column, other_table] {
            assert_eq!(sealer.open(aad, &sealed).unwrap_err(), SealError::Decrypt);
        }
    }

    #[test]
    fn associated_data_is_table_column_key() {
        assert_eq!(
            AAD.to_bytes(),
            b"claude_links/access_token_enc/67e55044-10b1-426f-9247-bb680e5fe0c8"
        );
    }

    #[test]
    fn a_wrong_key_fails() {
        let sealed = sealer().seal(AAD, &SecretString::from("secret")).unwrap();
        assert_eq!(sealer().open(AAD, &sealed).unwrap_err(), SealError::Decrypt);
    }

    #[test]
    fn tampering_fails() {
        let sealer = sealer();
        let sealed = sealer.seal(AAD, &SecretString::from("secret")).unwrap();
        for i in 1..sealed.len() {
            let mut tampered = sealed.clone();
            tampered[i] ^= 1;
            assert_eq!(
                sealer.open(AAD, &tampered).unwrap_err(),
                SealError::Decrypt,
                "byte {i}"
            );
        }
    }

    #[test]
    fn unknown_version_and_truncation_are_reported() {
        let sealer = sealer();
        let mut sealed = sealer.seal(AAD, &SecretString::from("secret")).unwrap();
        assert_eq!(
            sealer
                .open(AAD, &sealed[..HEADER_LEN + TAG_LEN - 1])
                .unwrap_err(),
            SealError::Truncated
        );
        assert_eq!(sealer.open(AAD, &[]).unwrap_err(), SealError::Truncated);
        sealed[0] = 2;
        assert_eq!(
            sealer.open(AAD, &sealed).unwrap_err(),
            SealError::UnknownVersion(2)
        );
    }

    #[test]
    fn non_utf8_plaintext_is_reported() {
        let sealer = sealer();
        let nonce = Nonce::<ChaCha20Poly1305>::default();
        let mut body = vec![0xff, 0xfe];
        let tag = sealer
            .cipher
            .encrypt_inout_detached(&nonce, &AAD.to_bytes(), body.as_mut_slice().into())
            .unwrap();
        let mut sealed = vec![KEY_VERSION];
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&body);
        sealed.extend_from_slice(&tag);
        assert_eq!(sealer.open(AAD, &sealed).unwrap_err(), SealError::NotUtf8);
    }

    #[test]
    fn keys_load_from_base64_with_surrounding_whitespace() {
        let key = Sealer::generate_key().unwrap();
        assert_eq!(STANDARD.decode(key.expose_secret()).unwrap().len(), KEY_LEN);
        let padded = SecretString::from(format!("  {}\n", key.expose_secret()));
        let sealed = Sealer::from_base64(&key)
            .unwrap()
            .seal(AAD, &SecretString::from("x"))
            .unwrap();
        let opened = Sealer::from_base64(&padded)
            .unwrap()
            .open(AAD, &sealed)
            .unwrap();
        assert_eq!(opened.expose_secret(), "x");
    }

    #[test]
    fn generated_keys_differ() {
        let a = Sealer::generate_key().unwrap();
        let b = Sealer::generate_key().unwrap();
        assert_ne!(a.expose_secret(), b.expose_secret());
    }

    #[test]
    fn bad_keys_are_rejected_without_echoing_them() {
        let err = Sealer::from_base64(&SecretString::from("not base64!")).unwrap_err();
        assert_eq!(err, KeyError::Base64);
        assert!(!err.to_string().contains("not base64!"));
        let short = SecretString::from(STANDARD.encode([7u8; 16]));
        assert_eq!(Sealer::from_base64(&short).unwrap_err(), KeyError::Length);
        let long = SecretString::from(STANDARD.encode([7u8; 33]));
        assert_eq!(Sealer::from_base64(&long).unwrap_err(), KeyError::Length);
    }

    #[test]
    fn debug_hides_the_key() {
        assert_eq!(format!("{:?}", sealer()), "Sealer { .. }");
    }
}
