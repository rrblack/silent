//! Authenticated encryption for everything peers send through the relay.
//!
//! ChaCha20-Poly1305 with a per-direction key and a 64-bit message counter as
//! the nonce. The relay delivers messages in order, so the receiver demands
//! exactly the next counter: replayed, reordered or dropped messages are
//! rejected rather than silently accepted.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

use crate::handshake::PeerId;

const AAD_PREFIX: &[u8] = b"silent/v1/seal";
const COUNTER_LEN: usize = 8;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SealError {
    #[error("message too short")]
    Truncated,
    #[error("message out of order (expected {expected}, got {got})")]
    OutOfOrder { expected: u64, got: u64 },
    #[error("message failed authentication")]
    Forged,
}

fn nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_be_bytes());
    Nonce::from(n)
}

fn aad(from: PeerId, to: PeerId) -> Vec<u8> {
    let mut a = AAD_PREFIX.to_vec();
    a.extend_from_slice(&from.to_be_bytes());
    a.extend_from_slice(&to.to_be_bytes());
    a
}

pub struct Sealer {
    cipher: ChaCha20Poly1305,
    aad: Vec<u8>,
    counter: u64,
}

impl Sealer {
    pub(crate) fn new(key: &[u8; 32], from: PeerId, to: PeerId) -> Self {
        Self { cipher: ChaCha20Poly1305::new(Key::from_slice(key)), aad: aad(from, to), counter: 0 }
    }

    pub fn seal(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let counter = self.counter;
        self.counter = self.counter.checked_add(1).expect("2^64 messages");
        let ciphertext = self
            .cipher
            .encrypt(&nonce(counter), Payload { msg: plaintext, aad: &self.aad })
            .expect("chacha20poly1305 encryption is infallible for in-memory buffers");
        let mut out = Vec::with_capacity(COUNTER_LEN + ciphertext.len());
        out.extend_from_slice(&counter.to_be_bytes());
        out.extend_from_slice(&ciphertext);
        out
    }
}

pub struct Opener {
    cipher: ChaCha20Poly1305,
    aad: Vec<u8>,
    expected: u64,
}

impl Opener {
    pub(crate) fn new(key: &[u8; 32], from: PeerId, to: PeerId) -> Self {
        Self { cipher: ChaCha20Poly1305::new(Key::from_slice(key)), aad: aad(from, to), expected: 0 }
    }

    pub fn open(&mut self, sealed: &[u8]) -> Result<Vec<u8>, SealError> {
        if sealed.len() < COUNTER_LEN + 16 {
            return Err(SealError::Truncated);
        }
        let (counter, ciphertext) = sealed.split_at(COUNTER_LEN);
        let counter = u64::from_be_bytes(counter.try_into().expect("split at 8"));
        if counter != self.expected {
            return Err(SealError::OutOfOrder { expected: self.expected, got: counter });
        }
        let plaintext = self
            .cipher
            .decrypt(&nonce(counter), Payload { msg: ciphertext, aad: &self.aad })
            .map_err(|_| SealError::Forged)?;
        self.expected += 1;
        Ok(plaintext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (Sealer, Opener) {
        let key = [7u8; 32];
        (Sealer::new(&key, 1, 2), Opener::new(&key, 1, 2))
    }

    #[test]
    fn roundtrip_in_order() {
        let (mut s, mut o) = pair();
        for i in 0..10u8 {
            assert_eq!(o.open(&s.seal(&[i; 5])).unwrap(), vec![i; 5]);
        }
    }

    #[test]
    fn rejects_replay_and_reorder() {
        let (mut s, mut o) = pair();
        let m0 = s.seal(b"a");
        let m1 = s.seal(b"b");
        assert_eq!(o.open(&m1), Err(SealError::OutOfOrder { expected: 0, got: 1 }));
        o.open(&m0).unwrap();
        assert_eq!(o.open(&m0), Err(SealError::OutOfOrder { expected: 1, got: 0 }));
        o.open(&m1).unwrap();
    }

    #[test]
    fn rejects_tampering() {
        let (mut s, mut o) = pair();
        let mut m = s.seal(b"hello");
        let last = m.len() - 1;
        m[last] ^= 1;
        assert_eq!(o.open(&m), Err(SealError::Forged));
        assert_eq!(o.open(&m[..4]), Err(SealError::Truncated));
    }

    #[test]
    fn rejects_wrong_direction() {
        let key = [7u8; 32];
        let mut s = Sealer::new(&key, 1, 2);
        let mut o = Opener::new(&key, 2, 1);
        assert_eq!(o.open(&s.seal(b"x")), Err(SealError::Forged));
    }

    #[test]
    fn failed_open_does_not_advance() {
        let (mut s, mut o) = pair();
        let mut forged = s.seal(b"a");
        forged[COUNTER_LEN] ^= 1;
        assert!(o.open(&forged).is_err());
        let (mut s2, _) = pair();
        assert_eq!(o.open(&s2.seal(b"a")).unwrap(), b"a");
    }
}
