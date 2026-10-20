//! Protocol and cryptography for Silent. No I/O lives here, so everything in
//! this crate is deterministic and unit-testable.

pub mod code;
pub mod handshake;
pub mod proto;
pub mod sas;
pub mod seal;
mod words;

pub use code::{CodeError, InviteCode};
pub use handshake::{PairHandshake, PairKeys, PeerId};
pub use sas::fingerprint_from_sdp;
pub use seal::{Opener, SealError, Sealer};
pub use words::WORDS;
