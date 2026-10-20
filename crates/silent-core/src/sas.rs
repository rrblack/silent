//! Safety words: a short authentication string friends can read aloud.
//!
//! Derived from the pairwise PAKE key and both peers' DTLS certificate
//! fingerprints. Someone sitting in the middle (even one who learned the
//! code) ends up with different keys on each side, so the words would not
//! match. Matching words prove the media connection itself is end to end.

use hkdf::Hkdf;
use sha2::Sha256;

use crate::words::WORDS;

pub(crate) fn safety_words(sas_key: &[u8; 32], mine: &str, theirs: &str) -> [&'static str; 4] {
    let mine = normalize(mine);
    let theirs = normalize(theirs);
    let (lo, hi) = if mine <= theirs {
        (mine, theirs)
    } else {
        (theirs, mine)
    };
    let mut info = b"silent/v1/sas".to_vec();
    info.extend_from_slice(lo.as_bytes());
    info.push(0);
    info.extend_from_slice(hi.as_bytes());

    let hk = Hkdf::<Sha256>::from_prk(sas_key).expect("32-byte prk");
    let mut out = [0u8; 4];
    hk.expand(&info, &mut out).expect("valid length");
    out.map(|b| WORDS[b as usize])
}

fn normalize(fp: &str) -> String {
    fp.trim().to_ascii_uppercase()
}

/// Pulls the DTLS fingerprint (`a=fingerprint:sha-256 AB:CD:...`) out of an
/// SDP blob. Session-level and media-level attributes are both accepted; a
/// single connection only ever carries one certificate.
pub fn fingerprint_from_sdp(sdp: &str) -> Option<String> {
    sdp.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("a=fingerprint:")?;
        let (algo, value) = rest.split_once(' ')?;
        Some(format!(
            "{} {}",
            algo.to_ascii_lowercase(),
            value.trim().to_ascii_uppercase()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_independent() {
        let key = [3u8; 32];
        assert_eq!(
            safety_words(&key, "aa", "BB"),
            safety_words(&key, "bb", "AA")
        );
    }

    #[test]
    fn changes_with_fingerprint_or_key() {
        let key = [3u8; 32];
        let base = safety_words(&key, "aa", "bb");
        assert_ne!(base, safety_words(&key, "aa", "bc"));
        assert_ne!(base, safety_words(&[4u8; 32], "aa", "bb"));
    }

    #[test]
    fn parses_fingerprint() {
        let sdp =
            "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\na=fingerprint:SHA-256 ab:cd:ef\r\nm=audio 9\r\n";
        assert_eq!(
            fingerprint_from_sdp(sdp).as_deref(),
            Some("sha-256 AB:CD:EF")
        );
        assert_eq!(fingerprint_from_sdp("v=0\r\n"), None);
    }
}
