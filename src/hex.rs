//! Lower-case hex, the one encoding of every digest, key and random id
//! rkat-tui writes (realm ids, trust record names, endpoint tokens and
//! state keys, handshake nonces, idempotency keys, cursor daemon ids), and
//! the one place that draws bytes from the OS.

use std::fmt::Write as _;
use std::io::Read as _;

use sha2::Digest as _;

/// `bytes` as lower-case hex, two digits each.
#[must_use]
pub fn encode(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// The SHA-256 of `bytes`, in lower-case hex.
#[must_use]
pub fn sha256(bytes: &[u8]) -> String {
    encode(&sha2::Sha256::digest(bytes))
}

/// `N` bytes from the OS's random source.
///
/// # Errors
///
/// When `/dev/urandom` cannot be read.
pub fn random_bytes<const N: usize>() -> std::io::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// `N` random bytes from the OS, hex-encoded (`2 * N` digits).
///
/// # Errors
///
/// When `/dev/urandom` cannot be read.
pub fn random<const N: usize>() -> std::io::Result<String> {
    Ok(encode(&random_bytes::<N>()?))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn hex_is_lower_case_two_digits_a_byte() {
        assert_eq!(encode(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
        assert_eq!(
            sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let id = random::<8>().unwrap();
        assert_eq!(id.len(), 16);
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert_ne!(id, random::<8>().unwrap());
    }
}
