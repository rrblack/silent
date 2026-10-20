//! Invite codes.
//!
//! A code looks like `4071-amber-falcon-river`. The leading number is the
//! *nameplate*: the only part the relay ever sees, used to put peers in the
//! same room. The words are the secret. They never leave the device and are
//! used solely as the PAKE password, so the relay cannot learn them and an
//! attacker gets exactly one online guess per handshake attempt.

use std::fmt;
use std::str::FromStr;

use rand::Rng;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::words::WORDS;

pub const SECRET_WORDS: usize = 3;
pub const NAMEPLATE_MAX: u16 = 9999;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CodeError {
    #[error("code must look like 1234-word-word-word")]
    Format,
    #[error("room number must be between 1 and {NAMEPLATE_MAX}")]
    Nameplate,
    #[error("unknown word '{0}'")]
    UnknownWord(String),
}

#[derive(Clone, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct InviteCode {
    #[zeroize(skip)]
    nameplate: u16,
    words: [u8; SECRET_WORDS],
}

impl InviteCode {
    pub fn generate() -> Self {
        let mut rng = rand::rngs::OsRng;
        let nameplate = rng.gen_range(1..=NAMEPLATE_MAX);
        let mut words = [0u8; SECRET_WORDS];
        rng.fill(&mut words);
        Self { nameplate, words }
    }

    pub fn nameplate(&self) -> u16 {
        self.nameplate
    }

    /// The PAKE password. Includes the nameplate so a code is only valid for
    /// the room it was issued for.
    pub fn password(&self) -> Vec<u8> {
        self.to_string().into_bytes()
    }

    /// Same secret, fresh nameplate. Used when the relay reports that the
    /// randomly chosen room number is already taken.
    pub fn with_new_nameplate(&self) -> Self {
        let mut next = self.clone();
        next.nameplate = rand::rngs::OsRng.gen_range(1..=NAMEPLATE_MAX);
        next
    }
}

impl fmt::Display for InviteCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.nameplate)?;
        for w in self.words {
            write!(f, "-{}", WORDS[w as usize])?;
        }
        Ok(())
    }
}

impl fmt::Debug for InviteCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InviteCode({}-***)", self.nameplate)
    }
}

impl FromStr for InviteCode {
    type Err = CodeError;

    /// Lenient parsing: ignores case and surrounding whitespace and accepts
    /// spaces, dots or underscores as separators, since people retype codes.
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let normalized = input.trim().to_ascii_lowercase();
        let mut parts = normalized
            .split(|c: char| c == '-' || c == ' ' || c == '.' || c == '_')
            .filter(|p| !p.is_empty());

        let nameplate: u16 = parts
            .next()
            .ok_or(CodeError::Format)?
            .parse()
            .map_err(|_| CodeError::Format)?;
        if nameplate == 0 || nameplate > NAMEPLATE_MAX {
            return Err(CodeError::Nameplate);
        }

        let mut words = [0u8; SECRET_WORDS];
        for slot in words.iter_mut() {
            let word = parts.next().ok_or(CodeError::Format)?;
            let index = WORDS
                .iter()
                .position(|w| *w == word)
                .ok_or_else(|| CodeError::UnknownWord(word.to_string()))?;
            *slot = index as u8;
        }
        if parts.next().is_some() {
            return Err(CodeError::Format);
        }
        Ok(Self { nameplate, words })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for _ in 0..200 {
            let code = InviteCode::generate();
            let parsed: InviteCode = code.to_string().parse().unwrap();
            assert_eq!(parsed, code);
            assert!((1..=NAMEPLATE_MAX).contains(&code.nameplate()));
        }
    }

    #[test]
    fn lenient_input() {
        let code: InviteCode = "  4071 Amber.FALCON_river ".parse().unwrap();
        assert_eq!(code.to_string(), "4071-amber-falcon-river");
    }

    #[test]
    fn rejects_bad_codes() {
        assert_eq!("".parse::<InviteCode>(), Err(CodeError::Format));
        assert_eq!("0-amber-falcon-river".parse::<InviteCode>(), Err(CodeError::Nameplate));
        assert_eq!("10000-amber-falcon-river".parse::<InviteCode>(), Err(CodeError::Nameplate));
        assert_eq!("12-amber-falcon".parse::<InviteCode>(), Err(CodeError::Format));
        assert_eq!("12-amber-falcon-river-oak".parse::<InviteCode>(), Err(CodeError::Format));
        assert_eq!(
            "12-amber-falcon-zzz".parse::<InviteCode>(),
            Err(CodeError::UnknownWord("zzz".into()))
        );
    }

    #[test]
    fn debug_hides_secret() {
        let code: InviteCode = "4071-amber-falcon-river".parse().unwrap();
        assert_eq!(format!("{code:?}"), "InviteCode(4071-***)");
    }

    #[test]
    fn new_nameplate_keeps_secret() {
        let code = InviteCode::generate();
        let moved = code.with_new_nameplate();
        assert_eq!(code.words, moved.words);
    }
}
