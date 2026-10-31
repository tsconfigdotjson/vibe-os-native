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
| `crates/vibe-env` | Environment backends behind `Compartment`, `Limiter`, `Sleeper`: the `linux` VMs on a Mac (`vibe_env::linux`, driving `vibe-vm` and `vibe-guest`), with the images they boot (`vibe_env::images`) |
| `crates/vibe-shim` | Holds one agent's terminal |
| `crates/vibe-guest` | PID 1 in each `linux` VM; a static `aarch64-unknown-linux-musl` binary. Its library is the protocol `vibed` and `vibe-shim` speak to it |
| `crates/vibe-testguest` | PID 1 of the small test guest `vibe-vm` is tested with, and a stand-in for Claude in the one `vibe-guest` is; not shipped |
| `crates/vibe-mcp` | The MCP server |
| `swift/VibeOS` | `VibeOS.app`, the SwiftUI menu bar app, and `VibeRPC`, its socket client |
| `swift/vibe-vm` | `vibe-vm`, the Virtualization.framework helper |
| `protocol/` | The socket contract both sides are tested against, `vibe-vm`'s in `protocol/vibe-vm/` and `vibe-guest`'s in `protocol/vibe-guest/`; see the READMEs |
| `scripts/` | `check-full.sh` runs every check; `sign-macos.sh` signs for local use; `test-guest.sh` builds the test guest; `base-image.sh` builds the Linux base image (with Docker); `vm-reboot-test.sh` checks a saved VM survives a reboot; `host-bundle.sh` builds the Linux `vibed` that Add Linux Host installs |

Every crate builds on macOS and Linux (x86_64 and arm64); `vibe-guest` and
`vibe-testguest` do their work only as static `aarch64-unknown-linux-musl`
binaries in a VM. The Swift packages are macOS 26 only.

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

scripts/test-guest.sh           # build/test-guest: small Linux guests for vibe-vm and vibe-guest
cargo build -p vibe-shim        # the vibe-guest test attaches through a real shim
VIBE_VM_BIN=build/vibe-vm VIBE_VM_GUEST=build/test-guest \
  cargo test -p vibe-env --test vm_cycle -- --test-threads=1 --nocapture
```

linux agents on a Mac boot the base image and Claude Code's managed layer,
which `vibed` downloads into `~/VibeOS/linux/` when it starts, and sign in to
Claude with the token `vibe setup-token` keeps in the Keychain. Until a base
release is published, build one and point `vibed` at it:

```sh
scripts/base-image.sh           # build/base-image/base-2: kernel, initrd, Debian image
target/debug/vibe setup-token   # the Claude login for linux agents
VIBE_BASE_RELEASES=file://$PWD/build/base-image target/debug/vibed
```

Add Linux Host installs the Linux builds of `vibed`, `vibe` and `vibe-shim`
over SSH. A release carries them in the app; for a build of your own, make
them and point `vibed` at them:

```sh
scripts/host-bundle.sh          # build/host-bundle/{x86_64,aarch64}: static musl builds (needs zig, cargo-zigbuild)
VIBE_HOST_BUNDLE=$PWD/build/host-bundle target/debug/vibed
target/debug/vibe host add vps me@vps.example.com   # or Add Linux Host… in the menu
```

On the server, `vibe project`, `vibe profile` and `vibe key` set it up
without the menu bar, and it serves its own MCP when the Mac is off.

A Linux host with a public DNS name can be the external address: outside MCP
clients reach agents on it and on the Mac at `https://<name>/mcp` with an
API key, and the Mac keeps an SSH tunnel to it, so nothing opens on the Mac.

```sh
target/debug/vibe host external vps vps.example.com   # or Settings → Hosts → Use as External Address
```

The server gets its certificate from Let's Encrypt on port 443, which
`vibed`, running as you, may use once the server allows it (as root:
`echo 'net.ipv4.ip_unprivileged_port_start=443' | sudo tee /etc/sysctl.d/60-vibeos.conf && sudo sysctl --system`).

`VIBE_SOCKET` (or `--socket`) points `vibed`, `vibe` and the app at another
socket. The app's rows attach Terminal with `/usr/local/bin/vibe`, or
`$VIBE_CLI` when it's set, such as `VIBE_CLI=$PWD/target/debug/vibe`.

## Check

```sh
scripts/check-full.sh
```

One command for everything: Rust fmt, clippy, build and test; the
`vibe-guest` cross-build; a `vibed` smoke test; on a Mac, both Swift packages,
the Swift contract tests, signing with the virtualization entitlement,
smoke tests of `vibe-vm` and `VibeOS.app` against a live `vibed`, the menu
following a live agent and two clicks giving one Terminal tab; a Linux host
added over SSH and linked, on stand-ins for `ssh` and `systemctl` (and on a
real server with `VIBE_TEST_LINUX_HOST` set), and external access through it
(with a real certificate and tunnel when `VIBE_TEST_EXTERNAL_DOMAIN` names
it too); real
VMs booted, saved and restored through `vibe-vm`, the Linux base image, and
`linux` agents on it: the backend's conformance suite, and real Claude across
a sleep. It prints a
PASS/FAIL table and leaves logs in `build/check/`. On Linux the macOS steps
are skipped. CI runs the same script.

A saved VM must also survive a reboot of the Mac. That takes a reboot, so
it's two commands around one: `scripts/vm-reboot-test.sh save`, reboot,
then `scripts/vm-reboot-test.sh restore`.
