//! Relay grants, on the Mac (PRD: MCP interface, Events): what a Linux host keeps in place
//! of a caller's API key so that the webhook subscriptions that key made there hear this
//! Mac's events (`tunnel.grant`, then `tunnel.events`), with no caller around.
//!
//! A grant is `vgr1.<expires>.<key hash>.<mac>`: the hash of one of this Mac's own keys,
//! when it runs out, and an HMAC of both under a secret only this Mac holds, in memory: a
//! restart of this `vibed` ends every grant, and a host takes a fresh one at its keys' next
//! subscribe or refresh. A grant holds while it's unexpired and its key is still one of
//! this Mac's, so revoking the key ends it. It opens `tunnel.events` and nothing else, so a
//! host's memory gives up, at worst, which agents run here and how they stand, never a way
//! to drive them.

use vibe_core::keys::{hex, hmac_sha256, same_secret};
use vibe_core::rpc::{ErrorCode, ErrorObject, TunnelGrant};

use crate::core::{Core, error};

/// How long a grant holds: longer than the longest webhook subscription a host grants
/// (30 days), so one taken at a refresh outlasts what it's for.
pub(crate) const GRANT_FOR: i64 = 60 * 24 * 3600;

const PREFIX: &str = "vgr1";

fn mac_of(secret: &[u8], expires: i64, hash: &str) -> String {
    hex(&hmac_sha256(
        secret,
        &[
            b"vibeos relay grant\n",
            expires.to_string().as_bytes(),
            b"\n",
            hash.as_bytes(),
        ],
    ))
}

/// A grant's parts, if it's shaped like one.
fn parts(grant: &str) -> Option<(i64, &str, &str)> {
    let mut it = grant.split('.');
    let (prefix, expires, hash, mac) = (it.next()?, it.next()?, it.next()?, it.next()?);
    if prefix != PREFIX || it.next().is_some() {
        return None;
    }
    let hashy = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
    hashy.then_some((expires.parse().ok()?, hash, mac))
}

impl Core {
    /// The secret grants are signed with, made on first use.
    fn grant_secret(&self) -> Result<[u8; 32], ErrorObject> {
        if let Some(s) = self.relay_grant_secret.get() {
            return Ok(*s);
        }
        let fresh = vibe_core::keys::random_bytes().map_err(|e| {
            error(
                ErrorCode::InternalError,
                format!("can't make relay grants' secret: {e}"),
            )
        })?;
        // Another call's, if it made one just now.
        Ok(*self.relay_grant_secret.get_or_init(|| fresh))
    }

    /// A grant for this Mac's own key with this hash.
    pub(crate) fn issue_grant(&self, hash: &str) -> Result<TunnelGrant, ErrorObject> {
        let secret = self.grant_secret()?;
        let expires = self.now() + GRANT_FOR;
        Ok(TunnelGrant {
            grant: format!(
                "{PREFIX}.{expires}.{hash}.{}",
                mac_of(&secret, expires, hash)
            ),
            expires,
        })
    }

    /// Whether `grant` holds now: this Mac's, unexpired, and for one of its own keys.
    pub(crate) fn grant_holds(&self, grant: &str) -> bool {
        let Some((expires, hash, mac)) = parts(grant) else {
            return false;
        };
        let Ok(secret) = self.grant_secret() else {
            return false;
        };
        if !same_secret(mac.as_bytes(), mac_of(&secret, expires, hash).as_bytes())
            || expires <= self.now()
        {
            return false;
        }
        matches!(self.store().key_by_hash(hash), Ok(Some(false)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_are_shaped() {
        let hash = "a".repeat(64);
        let g = format!("vgr1.123.{hash}.ff");
        assert_eq!(parts(&g), Some((123, hash.as_str(), "ff")));
        for bad in [
            format!("vgr2.123.{hash}.ff"),
            format!("vgr1.x.{hash}.ff"),
            "vgr1.123.short.ff".to_owned(),
            format!("vgr1.123.{hash}.ff.more"),
            format!("vgr1.123.{hash}"),
            String::new(),
        ] {
            assert!(parts(&bad).is_none(), "{bad}");
        }
        // The MAC covers both.
        let s = [9u8; 32];
        assert_ne!(mac_of(&s, 1, &hash), mac_of(&s, 2, &hash));
        assert_ne!(mac_of(&s, 1, &hash), mac_of(&s, 1, &"b".repeat(64)));
        assert_ne!(mac_of(&s, 1, &hash), mac_of(&[8u8; 32], 1, &hash));
    }
}
