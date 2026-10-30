# VibeOS Native

Turns an always-on Apple Silicon Mac into a private host for Claude Code
agents, each in its own git worktree and compartment, driven over MCP and
watched from the menu bar. [PRD.md](PRD.md) is the design and the source of
truth.

## Layout

| Path | What |
| --- | --- |
| `crates/vibed` | The daemon. Owns all state; serves the JSON-RPC socket |
| `crates/vibe-core` | Domain types, the store, and the socket contract (`vibe_core::rpc`) |
| `crates/vibe-cli` | The `vibe` command |
| `crates/vibe-status` | The status engine (agent-tail) |
| `crates/vibe-env` | Environment backends behind `Compartment`, `Limiter`, `Sleeper` |
| `crates/vibe-shim` | Holds one agent's terminal |
| `crates/vibe-guest` | PID 1 in each `linux` VM; a static `aarch64-unknown-linux-musl` binary |
| `crates/vibe-mcp` | The MCP server |
| `swift/VibeOS` | `VibeOS.app`, the SwiftUI menu bar app, and `VibeRPC`, its socket client |
| `swift/vibe-vm` | `vibe-vm`, the Virtualization.framework helper |
| `protocol/` | The socket contract both sides are tested against; see its README |
| `scripts/` | `check-full.sh` runs every check; `sign-macos.sh` signs for local use |

Every crate but `vibe-guest` builds on macOS and Linux (x86_64 and arm64);
the Swift packages are macOS 26 only.

## Develop

You need rustup; on a Mac, also Xcode 26 or its command line tools.
`rust-toolchain.toml` pins the toolchain and adds the musl target.

```sh
cargo build                     # everything in the workspace
cargo test
target/debug/vibed &            # the hello-world daemon on its default socket
target/debug/vibe hello         # vibed 0.1.0 · protocol 1 · macos/aarch64 · pid …
cargo build -p vibe-guest --target aarch64-unknown-linux-musl

scripts/sign-macos.sh           # build/VibeOS.app and build/vibe-vm, ad hoc signed
open build/VibeOS.app
```

`VIBE_SOCKET` (or `--socket`) points `vibed`, `vibe` and the app at another
socket.

## Check

```sh
scripts/check-full.sh
```

One command for everything: Rust fmt, clippy, build and test; the
`vibe-guest` cross-build; a `vibed` smoke test; on a Mac, both Swift packages,
the Swift contract tests, signing with the virtualization entitlement, and
smoke tests of `vibe-vm` and `VibeOS.app` against a live `vibed`. It prints a
PASS/FAIL table and leaves logs in `build/check/`. On Linux the macOS steps
are skipped. CI runs the same script.
