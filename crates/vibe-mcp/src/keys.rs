//! The keys `vibe_send` presses, by name, as the bytes a terminal sends for
//! each (as VibeOS names them): one character, or `Enter`, `Escape`, `Tab`,
//! `BackTab`, `Backspace`, `Delete`, `Space`, `Up`, `Down`, `Left`,
//! `Right`, `Home`, `End`, `PageUp`, `PageDown`, or `Ctrl+<letter>`.

/// The names `vibe_send` takes, besides a single character and
/// `Ctrl+<letter>`.
pub const NAMED: &[(&str, &[u8])] = &[
    ("Enter", b"\r"),
    ("Escape", b"\x1b"),
    ("Tab", b"\t"),
    ("BackTab", b"\x1b[Z"),
    ("Backspace", b"\x7f"),
    ("Delete", b"\x1b[3~"),
    ("Space", b" "),
    ("Up", b"\x1b[A"),
    ("Down", b"\x1b[B"),
    ("Right", b"\x1b[C"),
    ("Left", b"\x1b[D"),
    ("Home", b"\x1b[H"),
    ("End", b"\x1b[F"),
    ("PageUp", b"\x1b[5~"),
    ("PageDown", b"\x1b[6~"),
];

/// The bytes for the key called `name`. Names are matched ignoring case;
/// a single character is itself.
pub fn bytes(name: &str) -> Result<Vec<u8>, String> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.clone().next()) {
        return Ok(c.to_string().into_bytes());
    }
    if let Some((_, b)) = NAMED.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
        return Ok(b.to_vec());
    }
    let ctrl = ["ctrl+", "ctrl-", "c-"].iter().find_map(|p| {
        name.get(..p.len())
            .filter(|head| head.eq_ignore_ascii_case(p))
            .map(|_| &name[p.len()..])
    });
    if let Some(rest) = ctrl {
        let mut c = rest.chars();
        if let (Some(c), None) = (c.next(), c.next())
            && c.is_ascii_alphabetic()
        {
            return Ok(vec![c.to_ascii_lowercase() as u8 - b'a' + 1]);
        }
    }
    Err(format!(
        "unknown key {name:?}: keys are one character, Ctrl+<letter>, or one of {}",
        NAMED.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_the_bytes_a_terminal_sends() {
        assert_eq!(bytes("2").unwrap(), b"2");
        assert_eq!(bytes("y").unwrap(), b"y");
        assert_eq!(bytes("é").unwrap(), "é".as_bytes());
        assert_eq!(bytes("Enter").unwrap(), b"\r");
        assert_eq!(bytes("enter").unwrap(), b"\r");
        assert_eq!(bytes("Escape").unwrap(), b"\x1b");
        assert_eq!(bytes("Down").unwrap(), b"\x1b[B");
        assert_eq!(bytes("BackTab").unwrap(), b"\x1b[Z");
        assert_eq!(bytes("Ctrl+C").unwrap(), b"\x03");
        assert_eq!(bytes("ctrl-a").unwrap(), b"\x01");
        assert_eq!(bytes("C-z").unwrap(), b"\x1a");
        for bad in ["", "Ctrl+", "Ctrl+1", "Ctrl+ab", "Enterr", "F13"] {
            let e = bytes(bad).unwrap_err();
            assert!(e.contains("unknown key"), "{bad}: {e}");
        }
    }
}
