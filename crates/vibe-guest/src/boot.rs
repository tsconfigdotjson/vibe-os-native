//! What `vibe-guest` mounts where, from the kernel command line (PRD:
//! Mounts). `vibed` appends [`Boot::to_cmdline`] to the rest of the VM's
//! `cmdline` in `vm.json`; the guest reads it back from `/proc/cmdline`.
//! Every word has a dot in its name, so the kernel keeps it out of PID 1's
//! arguments and environment.
//!
//! ```text
//! vibe.layers=managed,home,template,base   the read-only disks, top first
//! vibe.upper=agent                         the agent's writable disk
//! vibe.home=home                           a writable disk for `vibe`'s home
//!                                          alone: the toolbox's Linux home
//! vibe.sudo=1                              `vibe` may use sudo: the toolbox
//! vibe.share=worktree:/Users/me/VibeOS/worktrees/app/quiet-amber-otter
//! vibe.share=claude:/home/vibe/.claude     a share's tag, and where it goes
//! vibe.uid=501 vibe.gid=20                 who `vibe` is
//! ```
//!
//! Disks are named by their id in `vm.json`, which is their virtio serial;
//! shares by their tag. A path is percent-encoded where it has a space, a
//! `%` or anything else the command line can't carry.

use std::fmt::Write as _;

/// What vibe-guest reads from the command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Boot {
    /// The read-only layers, top first: overlayfs's lower directories. Empty
    /// means the guest runs from its initrd and stacks nothing.
    pub layers: Vec<String>,
    /// The disk that takes the agent's writes: overlayfs's upper directory.
    /// It's formatted on the first boot. Needs `layers`.
    pub upper: Option<String>,
    /// A writable disk that takes the writes to `vibe`'s home instead, in its
    /// own `upper/home/vibe`, so it can be stacked later as a layer of its
    /// own: the toolbox writes the Linux home this way, and the template on
    /// `upper`. Formatted on the first boot, like `upper`. Needs `upper`.
    pub home: Option<String>,
    /// Whether `vibe` may run anything as root with `sudo`, for the toolbox
    /// shell; agents never may.
    pub sudo: bool,
    pub shares: Vec<Share>,
    /// The uid and gid of `vibe`, who runs the agent: the host user's, so
    /// files in the shares, which virtio-fs shows with their host owner, are
    /// its own. Absent: 1000.
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

/// A virtio-fs share and where in the guest it's mounted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    pub tag: String,
    /// An absolute path in the guest.
    pub path: String,
}

/// The longest command line the kernel takes on arm64, with its NUL.
pub const MAX_CMDLINE: usize = 2048;

/// The uid and gid of `vibe` when the command line doesn't say.
pub const DEFAULT_ID: u32 = 1000;

impl Boot {
    /// The words for the kernel command line, space-separated.
    pub fn to_cmdline(&self) -> Result<String, String> {
        self.validate()?;
        let mut words = Vec::new();
        if !self.layers.is_empty() {
            words.push(format!("vibe.layers={}", self.layers.join(",")));
        }
        if let Some(upper) = &self.upper {
            words.push(format!("vibe.upper={upper}"));
        }
        if let Some(home) = &self.home {
            words.push(format!("vibe.home={home}"));
        }
        if self.sudo {
            words.push("vibe.sudo=1".into());
        }
        for share in &self.shares {
            words.push(format!("vibe.share={}:{}", share.tag, encode(&share.path)));
        }
        if let Some(uid) = self.uid {
            words.push(format!("vibe.uid={uid}"));
        }
        if let Some(gid) = self.gid {
            words.push(format!("vibe.gid={gid}"));
        }
        Ok(words.join(" "))
    }

    /// `base` with these words appended: a whole command line, checked
    /// against [`MAX_CMDLINE`].
    pub fn append_to(&self, base: &str) -> Result<String, String> {
        let ours = self.to_cmdline()?;
        let line = match (base.trim(), ours.as_str()) {
            (base, "") => base.to_owned(),
            ("", ours) => ours.to_owned(),
            (base, ours) => format!("{base} {ours}"),
        };
        if line.len() >= MAX_CMDLINE {
            return Err(format!(
                "the kernel command line would be {} bytes; it takes at most {}",
                line.len(),
                MAX_CMDLINE - 1
            ));
        }
        Ok(line)
    }

    pub fn validate(&self) -> Result<(), String> {
        let mut disks: Vec<&str> = Vec::new();
        for id in self.layers.iter().chain(&self.upper).chain(&self.home) {
            label("disk id", id)?;
            if disks.contains(&id.as_str()) {
                return Err(format!("disk `{id}` is stacked twice"));
            }
            disks.push(id);
        }
        if self.upper.is_some() && self.layers.is_empty() {
            return Err("a writable disk needs layers to stack it on".into());
        }
        if self.home.is_some() && self.upper.is_none() {
            return Err("a writable home needs a writable disk under it".into());
        }
        let mut tags: Vec<&str> = Vec::new();
        for share in &self.shares {
            label("share tag", &share.tag)?;
            if tags.contains(&share.tag.as_str()) {
                return Err(format!("share `{}` is mounted twice", share.tag));
            }
            tags.push(&share.tag);
            absolute(&share.path)?;
        }
        Ok(())
    }

    /// Reads the `vibe.` words of a command line, ignoring every other word.
    /// A word it can't read is a problem, not a failure: the guest boots
    /// with the rest and reports it.
    pub fn parse(cmdline: &str) -> (Boot, Vec<String>) {
        let mut boot = Boot::default();
        let mut problems = Vec::new();
        for word in cmdline.split_ascii_whitespace() {
            let Some((key, value)) = word.split_once('=') else {
                continue;
            };
            let result = match key {
                "vibe.layers" => {
                    boot.layers = value
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect();
                    Ok(())
                }
                "vibe.upper" => {
                    boot.upper = Some(value.to_owned());
                    Ok(())
                }
                "vibe.home" => {
                    boot.home = Some(value.to_owned());
                    Ok(())
                }
                "vibe.sudo" => match value {
                    "1" => {
                        boot.sudo = true;
                        Ok(())
                    }
                    "0" => Ok(()),
                    _ => Err("is 1 or 0".to_owned()),
                },
                "vibe.share" => parse_share(value).map(|s| boot.shares.push(s)),
                "vibe.uid" => parse_id(value).map(|id| boot.uid = Some(id)),
                "vibe.gid" => parse_id(value).map(|id| boot.gid = Some(id)),
                k if k.starts_with("vibe.") => Err(format!("unknown setting `{k}`")),
                _ => Ok(()),
            };
            if let Err(e) = result {
                problems.push(format!("the kernel command line: `{word}`: {e}"));
            }
        }
        if let Err(e) = boot.validate() {
            problems.push(format!("the kernel command line: {e}"));
            // Don't stack or mount anything from a line that doesn't make
            // sense; booting from the initrd still lets the host see why.
            boot.layers.clear();
            boot.upper = None;
            boot.home = None;
            let mut seen = Vec::new();
            boot.shares.retain(|s| {
                let first = !seen.contains(&s.tag);
                seen.push(s.tag.clone());
                first
            });
        }
        (boot, problems)
    }

    /// The uid of `vibe`.
    pub fn uid(&self) -> u32 {
        self.uid.unwrap_or(DEFAULT_ID)
    }

    pub fn gid(&self) -> u32 {
        self.gid.unwrap_or(DEFAULT_ID)
    }
}

fn parse_share(value: &str) -> Result<Share, String> {
    let (tag, path) = value.split_once(':').ok_or("a share is written TAG:PATH")?;
    let path = decode(path)?;
    label("share tag", tag)?;
    absolute(&path)?;
    Ok(Share {
        tag: tag.to_owned(),
        path,
    })
}

fn parse_id(value: &str) -> Result<u32, String> {
    value
        .parse()
        .map_err(|_| format!("`{value}` isn't a number"))
}

/// A disk id or share tag, as `vm.json` allows.
fn label(what: &str, s: &str) -> Result<(), String> {
    let ok = !s.is_empty()
        && s.len() <= 36
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{what} `{s}` must be 1 to 36 letters, digits, `.`, `_` and `-`"
        ))
    }
}

/// A clean absolute path: no `.` or `..` parts, which could climb out of
/// where it says, and not `/` itself.
fn absolute(path: &str) -> Result<(), String> {
    let clean = path.starts_with('/')
        && path != "/"
        && !path.contains('\0')
        && path.split('/').all(|p| p != "." && p != "..");
    if clean {
        Ok(())
    } else {
        Err(format!("`{path}` isn't a clean absolute path"))
    }
}

/// Bytes the command line carries as they are: printable ASCII, less what
/// it splits or quotes on and our own escape.
fn plain(b: u8) -> bool {
    b.is_ascii_graphic() && !matches!(b, b'%' | b'"' | b',' | b'\\')
}

fn encode(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for &b in path.as_bytes() {
        if plain(b) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn decode(s: &str) -> Result<String, String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| format!("a bad escape in `{s}`"))?;
            out.push(hex);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| format!("`{s}` isn't UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> Boot {
        Boot {
            layers: ["managed", "home", "template", "base"]
                .map(String::from)
                .into(),
            upper: Some("agent".into()),
            home: None,
            sudo: false,
            shares: vec![
                Share {
                    tag: "worktree".into(),
                    path: "/Users/me/VibeOS/worktrees/my app/quiet-amber-otter".into(),
                },
                Share {
                    tag: "claude".into(),
                    path: "/home/vibe/.claude".into(),
                },
            ],
            uid: Some(501),
            gid: Some(20),
        }
    }

    #[test]
    fn the_command_line_round_trips() {
        let line = example().append_to("console=hvc0 quiet").unwrap();
        assert_eq!(
            line,
            "console=hvc0 quiet vibe.layers=managed,home,template,base vibe.upper=agent \
             vibe.share=worktree:/Users/me/VibeOS/worktrees/my%20app/quiet-amber-otter \
             vibe.share=claude:/home/vibe/.claude vibe.uid=501 vibe.gid=20"
        );
        assert_eq!(Boot::parse(&line), (example(), vec![]));
        // Nothing to say says nothing.
        assert_eq!(
            Boot::default().append_to("console=hvc0"),
            Ok("console=hvc0".into())
        );
        assert_eq!(
            Boot::parse("console=hvc0 root=/dev/vda"),
            (Boot::default(), vec![])
        );
        assert_eq!((Boot::default().uid(), Boot::default().gid()), (1000, 1000));
    }

    #[test]
    fn the_toolbox_writes_its_home_apart_and_may_sudo() {
        let toolbox = Boot {
            layers: vec!["managed".into(), "base".into()],
            upper: Some("template".into()),
            home: Some("home".into()),
            sudo: true,
            ..Boot::default()
        };
        let line = toolbox.to_cmdline().unwrap();
        assert_eq!(
            line,
            "vibe.layers=managed,base vibe.upper=template vibe.home=home vibe.sudo=1"
        );
        assert_eq!(Boot::parse(&line), (toolbox, vec![]));
        assert_eq!(Boot::parse("vibe.sudo=0").0, Boot::default());
        let (boot, problems) = Boot::parse("vibe.sudo=yes");
        assert!(!boot.sudo);
        assert!(problems[0].contains("is 1 or 0"), "{problems:?}");
        // A home with nothing to stack it on isn't stacked, nor the rest.
        let (boot, problems) = Boot::parse("vibe.layers=base vibe.home=home");
        assert_eq!((boot.layers.len(), boot.home), (0, None));
        assert!(
            problems[0].contains("a writable home needs"),
            "{problems:?}"
        );
    }

    #[test]
    fn paths_with_odd_bytes_survive() {
        for path in [
            "/a b/c",
            "/100%/x",
            "/tab\there",
            "/quote\"d",
            "/ünï/çødé",
            "/a,b:c",
        ] {
            let mut boot = Boot::default();
            boot.shares.push(Share {
                tag: "t".into(),
                path: path.into(),
            });
            let line = boot.to_cmdline().unwrap();
            assert_eq!(line.split_ascii_whitespace().count(), 1, "{line}");
            assert!(line.is_ascii(), "{line}");
            assert_eq!(Boot::parse(&line).0, boot, "{line}");
        }
    }

    #[test]
    fn bad_settings_are_refused_on_the_host() {
        type Change = fn(&mut Boot);
        let cases: [(Change, &str); 10] = [
            (|b| b.layers.push("base".into()), "stacked twice"),
            (|b| b.upper = Some("base".into()), "stacked twice"),
            (|b| b.home = Some("agent".into()), "stacked twice"),
            (
                |b| {
                    b.upper = None;
                    b.home = Some("home2".into());
                },
                "a writable home needs",
            ),
            (|b| b.layers[0] = "a b".into(), "disk id"),
            (|b| b.layers.clear(), "needs layers"),
            (|b| b.shares[1].tag = "worktree".into(), "mounted twice"),
            (|b| b.shares[0].path = "rel/path".into(), "clean absolute"),
            (|b| b.shares[0].path = "/a/../etc".into(), "clean absolute"),
            (|b| b.shares[0].path = "/".into(), "clean absolute"),
        ];
        for (change, why) in cases {
            let mut b = example();
            change(&mut b);
            let err = b.to_cmdline().unwrap_err();
            assert!(err.contains(why), "{err}");
        }
        let long = Boot {
            shares: vec![Share {
                tag: "t".into(),
                path: format!("/{}", "x".repeat(MAX_CMDLINE)),
            }],
            ..Boot::default()
        };
        assert!(long.append_to("").unwrap_err().contains("at most 2047"));
    }

    #[test]
    fn the_guest_boots_past_words_it_cant_read() {
        let (boot, problems) = Boot::parse(
            "vibe.uid=x vibe.share=nopath vibe.share=t:%zz vibe.gid=7 vibe.colour=red \
             vibe.share=ok:/mnt/ok",
        );
        assert_eq!(boot.gid, Some(7));
        assert_eq!(boot.uid, None);
        assert_eq!(boot.shares.len(), 1);
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(problems[3].contains("unknown setting `vibe.colour`"));
        // A stack that doesn't make sense isn't stacked at all.
        let (boot, problems) = Boot::parse("vibe.layers=base,base vibe.upper=agent");
        assert_eq!((boot.layers.len(), boot.upper), (0, None));
        assert!(problems[0].contains("stacked twice"), "{problems:?}");
        let (boot, _) = Boot::parse("vibe.upper=agent");
        assert_eq!(boot.upper, None);
    }
}
