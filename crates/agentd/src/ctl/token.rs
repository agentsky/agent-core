//! agentctl tokens: minted per `claude` process, stored only as a digest.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::TryRng as _;
use rand::rngs::SysRng;
use secrecy::zeroize::Zeroize as _;
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use store::TokenHash;

/// Random bytes in a token.
const TOKEN_BYTES: usize = 32;

/// The longest bearer value the server hashes. A token is 43 characters;
/// anything much longer is refused without hashing.
pub(crate) const MAX_PRESENTED_LEN: usize = 256;

/// The agentctl token of one `claude` process, for its `AGENTCTL_TOKEN`.
///
/// It is 32 random bytes, base64url without padding. agentd stores only its
/// SHA-256 digest. `Debug` never prints it.
pub struct ProcessToken(SecretString);

impl ProcessToken {
    /// Draws a new token from the operating system's generator. `None` if
    /// the generator fails.
    pub(crate) fn generate() -> Option<Self> {
        let mut bytes = [0u8; TOKEN_BYTES];
        SysRng.try_fill_bytes(&mut bytes).ok()?;
        let token = URL_SAFE_NO_PAD.encode(bytes);
        bytes.zeroize();
        Some(Self(SecretString::from(token)))
    }

    /// The token, to put in the process's `AGENTCTL_TOKEN`.
    pub fn secret(&self) -> &SecretString {
        &self.0
    }

    /// The digest the store keeps.
    pub(crate) fn hash(&self) -> TokenHash {
        hash_token(self.0.expose_secret())
    }
}

impl fmt::Debug for ProcessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProcessToken([redacted])")
    }
}

/// The SHA-256 digest of a presented token.
pub(crate) fn hash_token(token: &str) -> TokenHash {
    TokenHash(Sha256::digest(token.as_bytes()).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_32_random_bytes_in_base64url() {
        let a = ProcessToken::generate().unwrap();
        let b = ProcessToken::generate().unwrap();
        let text = a.secret().expose_secret();
        assert_eq!(text.len(), 43);
        assert_eq!(URL_SAFE_NO_PAD.decode(text).unwrap().len(), 32);
        assert_ne!(text, b.secret().expose_secret());
    }

    #[test]
    fn the_hash_is_the_sha256_of_the_token_text() {
        assert_eq!(
            hash_token("abc").0,
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad
            ]
        );
        let token = ProcessToken::generate().unwrap();
        assert_eq!(token.hash(), hash_token(token.secret().expose_secret()));
    }

    #[test]
    fn debug_never_prints_the_token() {
        let token = ProcessToken::generate().unwrap();
        let debug = format!("{token:?}");
        assert!(!debug.contains(token.secret().expose_secret()), "{debug}");
    }
}
