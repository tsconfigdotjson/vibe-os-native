//! API keys: `vk_` plus 256 random bits. Only a key's SHA-256 is stored, and
//! its first characters, so people can tell keys apart (PRD: Auth is always
//! on).
//!
//! A plain hash is enough: a key is 256 random bits, so there's nothing to
//! guess, and no slow KDF is needed.

use std::borrow::Cow;
use std::fs::File;
use std::io::{self, Read};

use sha2::{Digest, Sha256};

pub const KEY_PREFIX: &str = "vk_";

/// The random bytes in a key.
pub const KEY_BYTES: usize = 32;

/// The length of the base32 part: 256 bits in 5-bit characters.
const KEY_CHARS: usize = (KEY_BYTES * 8).div_ceil(5);

/// How much of a key is kept to show it: `vk_` and 8 characters.
const SHOWN: usize = KEY_PREFIX.len() + 8;

/// Lowercase RFC 4648 base32: easy to select with a double click, no
/// look-alike case pairs.
const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// A key from 32 random bytes, e.g. from [`random_bytes`].
pub fn encode(bytes: &[u8; KEY_BYTES]) -> String {
    let mut out = String::with_capacity(KEY_PREFIX.len() + KEY_CHARS);
    out.push_str(KEY_PREFIX);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// Reads 32 bytes from the OS's random source.
pub fn random_bytes() -> io::Result<[u8; KEY_BYTES]> {
    let mut bytes = [0; KEY_BYTES];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// The stored form of a key: its SHA-256, in hex.
pub fn hash(key: &str) -> String {
    Sha256::digest(key.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The part of a key that's kept to show it, e.g. `vk_abcdefgh`.
pub fn shown_prefix(key: &str) -> String {
    key.chars().take(SHOWN).collect()
}

/// Whether `s` has the shape of a key.
pub fn is_key(s: &str) -> bool {
    s.len() == KEY_PREFIX.len() + KEY_CHARS
        && s.starts_with(KEY_PREFIX)
        && s[KEY_PREFIX.len()..].bytes().all(|b| ALPHABET.contains(&b))
}

/// Replaces anything longer than a shown prefix that's shaped like a key
/// with its shown prefix and `…`, so a key can't reach a log by accident.
pub fn redact(text: &str) -> Cow<'_, str> {
    if !text.contains(KEY_PREFIX) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(KEY_PREFIX) {
        out.push_str(&rest[..at]);
        let candidate = &rest[at..];
        let len = KEY_PREFIX.len()
            + candidate[KEY_PREFIX.len()..]
                .bytes()
                .take_while(|b| ALPHABET.contains(b))
                .count();
        if len > SHOWN {
            out.push_str(&candidate[..SHOWN]);
            out.push('…');
            rest = &candidate[len..];
        } else {
            out.push_str(KEY_PREFIX);
            rest = &candidate[KEY_PREFIX.len()..];
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_vk_and_52_base32_chars() {
        let k = encode(&[0; 32]);
        assert_eq!(k, format!("vk_{}", "a".repeat(52)));
        let k = encode(&[0xff; 32]);
        // 256 bits fill 51 characters and one bit of the last.
        assert_eq!(k, format!("vk_{}q", "7".repeat(51)));
        let bytes: [u8; 32] = std::array::from_fn(|i| i as u8);
        let k = encode(&bytes);
        assert_eq!(k, "vk_aaaqeayeaudaocajbifqydiob4ibceqtcqkrmfyydenbwha5dypq");
        assert!(is_key(&k));
        assert!(!is_key(&k[..k.len() - 1]));
        assert!(!is_key(&k.to_uppercase()));
        assert!(!is_key(&k.replacen("vk_", "vx_", 1)));
    }

    #[test]
    fn random_keys_differ() {
        let a = encode(&random_bytes().unwrap());
        let b = encode(&random_bytes().unwrap());
        assert_ne!(a, b);
        assert!(is_key(&a));
    }

    #[test]
    fn hash_is_sha256_hex() {
        assert_eq!(
            hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn shown_prefix_is_short() {
        let k = encode(&[0; 32]);
        assert_eq!(shown_prefix(&k), "vk_aaaaaaaa");
    }

    #[test]
    fn redact_hides_keys_only() {
        let k = encode(&[7; 32]);
        let line = format!("bearer {k} from vk_ client, {k}.");
        let shown = shown_prefix(&k);
        assert_eq!(
            redact(&line),
            format!("bearer {shown}… from vk_ client, {shown}….")
        );
        assert!(!redact(&line).contains(&k[SHOWN..]));
        assert!(matches!(redact("no keys here"), Cow::Borrowed(_)));
        assert_eq!(redact("vk_ab and vk_"), "vk_ab and vk_");
        // A shown prefix is already safe.
        assert_eq!(redact(&format!("({shown}…)")), format!("({shown}…)"));
    }
}
