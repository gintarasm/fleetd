//! Per-agent tokens (ADR 0072, spec §1): 32 random bytes from
//! `/dev/urandom`, hex-encoded in the agent's `FLEET_AGENT_TOKEN`. The store
//! keeps only a SHA-256 digest, and nothing logs the token itself.

use std::fmt;
use std::io::{self, Read};

use sha2::{Digest as _, Sha256};

/// Random bytes in a token.
pub const TOKEN_BYTES: usize = 32;

/// A token's SHA-256 digest, as stored.
pub type Digest = [u8; 32];

/// An agent's token. `Debug` does not show it.
#[derive(Clone, PartialEq, Eq)]
pub struct AgentToken(String);

impl fmt::Debug for AgentToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AgentToken(…)")
    }
}

impl AgentToken {
    /// A new token from `/dev/urandom`.
    pub fn generate() -> io::Result<AgentToken> {
        let mut bytes = [0u8; TOKEN_BYTES];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        Ok(AgentToken(hex(&bytes)))
    }

    /// The hex text the agent's environment carries.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn digest(&self) -> Digest {
        digest(&self.0)
    }
}

/// The digest of a token as a client sent it.
pub fn digest(token: &str) -> Digest {
    Sha256::digest(token.as_bytes()).into()
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[usize::from(b >> 4)] as char);
        s.push(DIGITS[usize::from(b & 0xf)] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_64_hex_digits_and_differ() {
        let a = AgentToken::generate().unwrap();
        let b = AgentToken::generate().unwrap();
        assert_eq!(a.expose().len(), 2 * TOKEN_BYTES);
        assert!(a.expose().bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn debug_hides_the_token() {
        let t = AgentToken::generate().unwrap();
        assert!(!format!("{t:?}").contains(t.expose()));
    }

    #[test]
    fn digest_is_sha256_of_the_text() {
        // SHA-256("abc").
        assert_eq!(
            hex(&digest("abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let t = AgentToken::generate().unwrap();
        assert_eq!(t.digest(), digest(t.expose()));
    }
}
