//! An agent's name: `main-<id>`, a short random id, stable for the
//! member's life.

use std::sync::atomic::{AtomicU64, Ordering};

/// What every agent's name starts with (its id follows).
pub const NAME_PREFIX: &str = "main-";

/// How many characters an agent's id has.
pub const ID_CHARS: usize = 4;

/// The characters an agent's id is drawn from.
const ID_ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";

/// Whether `name` is an agent's member identity: `main-<id>` (lower-case
/// letters and digits).
#[must_use]
pub fn is_agent(name: &str) -> bool {
    name.strip_prefix(NAME_PREFIX).is_some_and(|id| {
        !id.is_empty()
            && id.len() <= 16
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    })
}

/// A fresh agent name no `taken` name has.
#[must_use]
pub fn new_name(taken: impl Fn(&str) -> bool) -> String {
    loop {
        let name = format!("{NAME_PREFIX}{}", random_id());
        if !taken(&name) {
            return name;
        }
    }
}

/// [`ID_CHARS`] characters of [`ID_ALPHABET`], from the system's random
/// source (else from the clock and a counter: a name only needs to differ
/// from the taken ones, which [`new_name`] checks).
fn random_id() -> String {
    static FALLBACK: AtomicU64 = AtomicU64::new(0);
    let read = crate::hex::random_bytes::<ID_CHARS>();
    let mut bytes = read.as_ref().copied().unwrap_or([0; ID_CHARS]);
    if read.is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let mixed = u64::from(nanos) ^ FALLBACK.fetch_add(0x9e37_79b9, Ordering::Relaxed);
        bytes = mixed.to_le_bytes()[..ID_CHARS]
            .try_into()
            .unwrap_or([0; ID_CHARS]);
    }
    bytes
        .iter()
        .map(|byte| char::from(ID_ALPHABET[usize::from(*byte) % ID_ALPHABET.len()]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_names_are_main_and_a_short_id() {
        for _ in 0..64 {
            let name = new_name(|_| false);
            assert!(is_agent(&name), "{name}");
            assert_eq!(name.len(), NAME_PREFIX.len() + ID_CHARS, "{name}");
        }
        assert!(!is_agent("main"), "the profile, not an agent");
        for not in [
            "", "main-", "main-AB", "main-a b", "mainx", "helper", "main-a/b",
        ] {
            assert!(!is_agent(not), "{not}");
        }
        // A taken name is never drawn again.
        let first = new_name(|_| false);
        let other = new_name(|name| name == first);
        assert_ne!(first, other);
    }
}
