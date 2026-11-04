//! How an agent's MCP client reaches a shared service (PRD: Shared MCP
//! services). It knows nothing of which agent is calling, or of tokens:
//! those stay in the Mac's broker.
//!
//! ```text
//! MCP client ── stdio ── vibe mcp-proxy ── local socket ── hub ══ relay ══ broker
//! ```
//!
//! - [`proxy`] is the shim: `vibe mcp-proxy <service>`. It answers the
//!   handshake itself, so tools load and it never dies while the Mac is away.
//! - [`local`] is what the shim says on the local socket: one hello line,
//!   then JSON-RPC, one message per line.
//! - [`hub`] gathers a machine's shims onto one relay: in `vibe-guest` for a
//!   VM, in `vibed` on a Linux host. On the Mac the shim's socket reaches the
//!   broker in-process, so there's no hub.
//! - [`frame`] is the relay's framing, the same over a VM's vsock port and
//!   over `ssh <host> vibe services`.
//!
//! The Mac always dials: the relay is carried by a connection the Mac opened,
//! so nothing listens for a guest or a host to call in.

pub mod frame;
pub mod hub;
pub mod local;
pub mod proxy;

/// The name a service's MCP server goes by in an agent: its tools are
/// `mcp__vibe-linear__*` in Claude Code. The prefix is reserved, so a server
/// the owner configured themselves is never clobbered.
pub fn server_name(service: &str) -> String {
    format!("vibe-{service}")
}

/// The argument words that start the shim for `service`, after the `vibe`
/// program itself.
pub fn proxy_args(service: &str) -> [String; 2] {
    ["mcp-proxy".to_owned(), service.to_owned()]
}

/// The longest JSON-RPC message or line anything here carries, as on
/// `vibed`'s own socket.
pub const MAX_MESSAGE: usize = 16 << 20;

/// Reads one line, without its newline, of at most [`MAX_MESSAGE`] bytes.
/// `None` at the end of the stream.
pub(crate) fn read_line(r: &mut impl std::io::BufRead) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::{BufRead, Read};
    let mut line = Vec::new();
    let n = r
        .by_ref()
        .take(MAX_MESSAGE as u64 + 1)
        .read_until(b'\n', &mut line)?;
    if n == 0 {
        return Ok(None);
    }
    if line.last() == Some(&b'\n') {
        line.pop();
    } else if line.len() > MAX_MESSAGE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "a message longer than 16 MB",
        ));
    }
    Ok(Some(line))
}
