//! Pairwise authenticated key exchange.
//!
//! Every pair of peers in a room runs symmetric SPAKE2 over the relay, using
//! the invite code as the password. Both sides end up with the same key only
//! if they typed the same code; the relay (or anyone watching it) learns
//! nothing it could use to guess the code offline.
//!
//! The SPAKE2 identity binds the protocol version, the room and the two peer
//! ids, so a transcript cannot be replayed into another room or between a
//! different pair of peers.

use hkdf::Hkdf;
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::code::InviteCode;
use crate::seal::{Opener, Sealer};

pub type PeerId = u32;

const PROTOCOL: &[u8] = b"silent/v1";

#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    #[error("peer sent a malformed key exchange message")]
    Malformed,
}

pub struct PairHandshake {
    state: Spake2<Ed25519Group>,
    me: PeerId,
    them: PeerId,
}

impl PairHandshake {
    /// Starts the exchange and returns the message to send to `them`.
    pub fn start(code: &InviteCode, me: PeerId, them: PeerId) -> (Self, Vec<u8>) {
        let identity = pair_identity(code.nameplate(), me, them);
        let (state, outbound) = Spake2::<Ed25519Group>::start_symmetric(
            &Password::new(code.password()),
            &Identity::new(&identity),
        );
        (Self { state, me, them }, outbound)
    }

    pub fn finish(self, inbound: &[u8]) -> Result<PairKeys, HandshakeError> {
        let mut shared = self
            .state
            .finish(inbound)
            .map_err(|_| HandshakeError::Malformed)?;
        let keys = PairKeys::derive(&shared, self.me, self.them);
        shared.zeroize();
        Ok(keys)
    }
}

fn pair_identity(nameplate: u16, a: PeerId, b: PeerId) -> Vec<u8> {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    let mut id = Vec::with_capacity(PROTOCOL.len() + 10);
    id.extend_from_slice(PROTOCOL);
    id.extend_from_slice(&nameplate.to_be_bytes());
    id.extend_from_slice(&lo.to_be_bytes());
    id.extend_from_slice(&hi.to_be_bytes());
    id
}

/// Keys for one pair of peers. Each direction has its own key so nonces can
/// never collide between the two senders.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct PairKeys {
    #[zeroize(skip)]
    me: PeerId,
    #[zeroize(skip)]
    them: PeerId,
    send: [u8; 32],
    recv: [u8; 32],
    sas: [u8; 32],
}

impl PairKeys {
    fn derive(shared: &[u8], me: PeerId, them: PeerId) -> Self {
        let hk = Hkdf::<Sha256>::new(Some(PROTOCOL), shared);
        let mut lo_to_hi = [0u8; 32];
        let mut hi_to_lo = [0u8; 32];
        let mut sas = [0u8; 32];
        hk.expand(b"seal lo->hi", &mut lo_to_hi)
            .expect("valid length");
        hk.expand(b"seal hi->lo", &mut hi_to_lo)
            .expect("valid length");
        hk.expand(b"safety words", &mut sas).expect("valid length");
        let (send, recv) = if me < them {
            (lo_to_hi, hi_to_lo)
        } else {
            (hi_to_lo, lo_to_hi)
        };
        lo_to_hi.zeroize();
        hi_to_lo.zeroize();
        Self {
            me,
            them,
            send,
            recv,
            sas,
        }
    }

    pub fn split(&self) -> (Sealer, Opener) {
        (
            Sealer::new(&self.send, self.me, self.them),
            Opener::new(&self.recv, self.them, self.me),
        )
    }

    pub fn safety_words(&self, my_fingerprint: &str, their_fingerprint: &str) -> [&'static str; 4] {
        crate::sas::safety_words(&self.sas, my_fingerprint, their_fingerprint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(code_a: &InviteCode, code_b: &InviteCode) -> (PairKeys, PairKeys) {
        let (a, msg_a) = PairHandshake::start(code_a, 1, 2);
        let (b, msg_b) = PairHandshake::start(code_b, 2, 1);
        (a.finish(&msg_b).unwrap(), b.finish(&msg_a).unwrap())
    }

    #[test]
    fn same_code_can_talk() {
        let code = InviteCode::generate();
        let (ka, kb) = pair(&code, &code);
        let (mut seal_a, _) = ka.split();
        let (_, mut open_b) = kb.split();
        let sealed = seal_a.seal(b"hello");
        assert_eq!(open_b.open(&sealed).unwrap(), b"hello");
        assert_eq!(ka.safety_words("fa", "fb"), kb.safety_words("fb", "fa"));
    }

    #[test]
    fn wrong_code_cannot_talk() {
        let code = InviteCode::generate();
        let wrong: InviteCode = {
            let mut s = code.to_string();
            let last = s.rsplit('-').next().unwrap().to_string();
            let replacement = if last == "oak" { "owl" } else { "oak" };
            s.truncate(s.len() - last.len());
            s.push_str(replacement);
            s.parse().unwrap()
        };
        let (ka, kb) = pair(&code, &wrong);
        let (mut seal_a, _) = ka.split();
        let (_, mut open_b) = kb.split();
        assert!(open_b.open(&seal_a.seal(b"hello")).is_err());
    }

    #[test]
    fn transcript_is_bound_to_peer_pair() {
        let code = InviteCode::generate();
        let (a, msg_a) = PairHandshake::start(&code, 1, 2);
        let (c, msg_c) = PairHandshake::start(&code, 3, 1);
        let ka = a.finish(&msg_c).unwrap();
        let kc = c.finish(&msg_a).unwrap();
        let (mut seal_a, _) = ka.split();
        let (_, mut open_c) = kc.split();
        assert!(open_c.open(&seal_a.seal(b"hello")).is_err());
    }

    #[test]
    fn rejects_garbage() {
        let code = InviteCode::generate();
        let (a, _) = PairHandshake::start(&code, 1, 2);
        assert!(a.finish(b"not a spake2 message").is_err());
    }
}
