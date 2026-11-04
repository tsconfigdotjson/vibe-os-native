//! The little encoding the OAuth flows need.

use std::fmt::Write;

const STANDARD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL_SAFE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64(bytes: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(alphabet[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else if pad {
                out.push('=');
            }
        }
    }
    out
}

/// Base64url without padding, as PKCE wants.
pub(crate) fn base64url(bytes: &[u8]) -> String {
    base64(bytes, URL_SAFE, false)
}

/// Standard Base64, whose characters need no quoting in `security -i`.
pub(crate) fn base64_standard(bytes: &[u8]) -> String {
    base64(bytes, STANDARD, true)
}

pub(crate) fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut n = 0u32;
    let mut bits = 0;
    for c in text.bytes().filter(|c| *c != b'=') {
        let v = STANDARD.iter().position(|a| *a == c)? as u32;
        n = (n << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((n >> bits) as u8);
            n &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Percent-encodes all but RFC 3986's unreserved characters.
pub(crate) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = (bytes[i] == b'%')
            .then(|| s.get(i + 1..i + 3))
            .flatten()
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (hex, bytes[i]) {
            (Some(v), _) => {
                out.push(v);
                i += 3;
            }
            (None, b'+') => {
                out.push(b' ');
                i += 1;
            }
            (None, b) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `name=value&…`, for a query or a form.
pub(crate) fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The pairs of a query or a form.
pub(crate) fn parse_form(text: &str) -> Vec<(String, String)> {
    text.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

/// The query of a URL, as pairs: the fragment's not part of it.
pub(crate) fn query_of(url: &str) -> Vec<(String, String)> {
    let url = url.split('#').next().unwrap_or(url);
    url.split_once('?')
        .map(|(_, q)| parse_form(q))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_4648() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_standard(plain.as_bytes()), encoded);
            assert_eq!(base64_decode(encoded).unwrap(), plain.as_bytes());
        }
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
        assert_eq!(base64_decode("not base64!"), None);
    }

    #[test]
    fn forms_and_queries_read_back() {
        let encoded = form(&[("a b", "c&d=e"), ("u", "vibeos://oauth/callback")]);
        assert_eq!(encoded, "a%20b=c%26d%3De&u=vibeos%3A%2F%2Foauth%2Fcallback");
        assert_eq!(
            parse_form(&encoded),
            [
                ("a b".to_owned(), "c&d=e".to_owned()),
                ("u".to_owned(), "vibeos://oauth/callback".to_owned())
            ]
        );
        assert_eq!(
            query_of("vibeos://oauth/callback?code=x%2By&state=1+2#frag"),
            [
                ("code".to_owned(), "x+y".to_owned()),
                ("state".to_owned(), "1 2".to_owned())
            ]
        );
        assert!(query_of("vibeos://oauth/callback").is_empty());
    }
}
