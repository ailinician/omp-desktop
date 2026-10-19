//! OS-backed random credentials and fixed-length secret comparisons.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ring::rand::{SecureRandom, SystemRandom};

pub fn random_b64(bytes: usize) -> Result<String, String> {
    let mut random = vec![0; bytes];
    SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| "could not generate secure random bytes".to_string())?;
    Ok(URL_SAFE_NO_PAD.encode(random))
}

/// Compare every byte for equal-length inputs, without a mismatch shortcut.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .fold(0_u8, |different, (a, b)| different | (a ^ b))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_b64_has_the_requested_entropy_and_ct_eq_compares() {
        let a = random_b64(32).unwrap();
        assert_eq!(a.len(), 43); // 32 bytes base64url, no padding
        assert_ne!(a, random_b64(32).unwrap());
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }

    #[test]
    fn random_b64_is_url_safe_and_round_trips_requested_bytes() {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        for bytes in [0, 1, 9, 16, 32] {
            let encoded = random_b64(bytes).unwrap();
            assert!(encoded
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
            assert_eq!(URL_SAFE_NO_PAD.decode(encoded).unwrap().len(), bytes);
        }
        assert!(ct_eq(b"", b""));
        assert!(!ct_eq(b"abc", b"xbc"));
    }
}
