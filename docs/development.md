# Developing VibeOS Native

How the repo is laid out, and how to build, check and release it.
[PRD.md](../PRD.md) is the design and the source of truth. To use the app, the
[README](../README.md) is the place to start.

## Layout

| Path | What |
| --- | --- |
| `crates/vibed` | The daemon. Owns all state; serves the JSON-RPC socket |
| `crates/vibe-core` | Domain types, the store, and the socket contract (`vibe_core::rpc`) |
| `crates/vibe-cli` | The `vibe` command |
| `crates/vibe-status` | The status engine (agent-tail) |
| `crates/vibe-env` | Environment backends behind `Compartment`, `Limiter`, `Sleeper`: the `linux` VMs on a Mac (`vibe_env::linux`, driving `vibe-vm` and `vibe-guest`), with the images they boot (`vibe_env::images`); native agents under a governor in `macos` (`vibe_env::macos`); `linux` on a Linux host, in cgroups (`vibe_env::cgroups`); and a backend for tests (`vibe_env::testing`) |
| `crates/vibe-shim` | Holds one agent's terminal |
| `crates/vibe-guest` | PID 1 in each `linux` VM; a static `aarch64-unknown-linux-musl` binary. Its library is the protocol `vibed` and `vibe-shim` speak to it |
| `crates/vibe-testguest` | PID 1 of the small test guest `vibe-vm` is tested with, and a stand-in for Claude in the one `vibe-guest` is; not shipped |
| `crates/vibe-mcp` | The MCP server |
| `crates/vibe-services` | Shared MCP services' broker, in every `vibed`: one login per install on its owning host, access tokens for the others, the OAuth lifecycle, policy and audit |
| `crates/vibe-relay` | Their `vibe mcp-proxy` stdio shim, and the relay out of a VM; `std` only, as `vibe-guest` links it |
| `swift/VibeOS` | `VibeOS.app`, the SwiftUI menu bar app, and `VibeRPC`, its socket client |
| `swift/vibe-vm` | `vibe-vm`, the Virtualization.framework helper |
| `protocol/` | The socket contract both sides are tested against, `vibe-vm`'s in `protocol/vibe-vm/` and `vibe-guest`'s in `protocol/vibe-guest/`; see the READMEs |
| `scripts/` | `check-full.sh` runs every check; `advisories.sh` holds `Cargo.lock` to RustSec, by `deny.toml`'s rules; `sign-macos.sh` signs for local use; `test-guest.sh` builds the test guest; `base-image.sh` builds the Linux base image (with Docker); `vm-reboot-test.sh` checks a saved VM survives a reboot; `keep-host-state.sh` puts a real host back as it was after a test of it; `host-bundle.sh` builds the Linux `vibed` that Add Linux Host installs, with `zig-tools.sh` fetching what that takes on first use; `release-macos.sh` builds a release DMG; `sparkle.py` signs it for Sparkle and writes the appcast; `surfaces.sh` screenshots every surface of the app for design review; `app-icon.swift` renders the app icon from its SVG |

Every crate compiles for macOS and Linux (x86_64 and arm64), which `check-full.sh`
holds with `cargo check` for the targets it isn't running on; `vibe-guest` and
`vibe-testguest` do their work only as static `aarch64-unknown-linux-musl`
binaries in a VM. The Swift packages are macOS 26 only, and a release of the app is
for Apple Silicon.

## Develop

You need rustup; on a Mac, also Xcode 26 or its command line tools.
`rust-toolchain.toml` picks the stable channel, with rustfmt, clippy and the musl
target.

```sh
cargo build                     # everything in the workspace
cargo test
target/debug/vibed &            # the daemon on its default socket
target/debug/vibe hello         # vibed <version> · protocol <n> · macos/aarch64 · pid …
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
Claude with the token `vibe setup-token` keeps in the Keychain. To test a base
image of your own, build one and point `vibed` at it:

```sh
scripts/base-image.sh           # build/base-image/base-3: kernel, initrd, Debian image
VIBE_LOGIN_KEYCHAIN=allow target/debug/vibe setup-token   # the Claude login for linux agents
target/debug/vibe pi            # Pi: what's installed, and who it's signed in to
VIBE_LOGIN_KEYCHAIN=allow VIBE_BASE_RELEASES=file://$PWD/build/base-image target/debug/vibed
```

A build outside an app stays out of the login Keychain unless it's run with
`VIBE_LOGIN_KEYCHAIN=allow`, which `cargo run` can't pass on, as `cargo` forces
`never`, and which `vibed` doesn't pass on to its agents. Without it, a debug `vibed`
keeps shared services' logins in `services/` in its state directory, removed when a
later run uses the Keychain, and can't see a Claude login that's in the Keychain.

VibeOS signs nobody in to Pi. `vibe pi` reports its logins, and signing in is Pi's own
`/login`: run `pi` in `vibe toolbox` for `linux` agents on a Mac, and on the machine itself
otherwise.

Pi is pinned (`vibe_env::pi_release::PINNED`). Moving the pin means looking again
at what the new version reads, runs and writes in its agent dir, at what the login
broker accepts (`vibe_env::pi_logins`), and at the provider keys and screen
messages read in `vibe_env::pi` and `vibe_status::screen`, then running
`scripts/check-full.sh`'s `pi-e2e` with `PI_E2E_VERSION`.

Add Linux Host installs the Linux builds of `vibed`, `vibe` and `vibe-shim`
over SSH. A release carries them in the app; for a build of your own, make
them and point `vibed` at them:

```sh
scripts/host-bundle.sh          # build/host-bundle/{x86_64,aarch64}: static musl builds (fetches zig and cargo-zigbuild on first use)
VIBE_HOST_BUNDLE=$PWD/build/host-bundle target/debug/vibed
target/debug/vibe host add vps me@vps.example.com   # or Settings → Hosts → Add Linux Host…
```

On the server, `vibe project`, `vibe profile` and `vibe key` set it up
without the menu bar, and it serves its own MCP when the Mac is off.

`VIBE_SOCKET` (or `--socket`) points `vibed`, `vibe` and the app at another
socket. The app's rows attach Terminal with `/usr/local/bin/vibe`, or
`$VIBE_CLI` when it's set, such as `VIBE_CLI=$PWD/target/debug/vibe`.

## Check

```sh
scripts/check-full.sh
```

One command for everything: Rust fmt, clippy, build and test; `Cargo.lock`
against RustSec; the `vibe-guest` cross-build; `cargo check` for the other macOS and Linux
targets; a `vibed` smoke test; the registry, sessions, agents, MCP and shared services
against a live `vibed`; real Pi on a mock model; the `linux` backend on real cgroups; on a
Mac, the `macos` backend, both Swift packages,
the Swift contract tests, signing with the virtualization entitlement,
smoke tests of `vibe-vm` and `VibeOS.app` against a live `vibed`, the menu
following a live agent and two clicks giving one Terminal tab; a Linux host
added over SSH and linked, on stand-ins for `ssh` and `systemctl` (and on a
real server with `VIBE_TEST_LINUX_HOST` set), and that host's MCP reaching the Mac's agents
through a stand-in `ssh -R`, and answering for its own with the Mac gone; real
VMs booted, saved and restored through `vibe-vm`, the Linux base image, and
`linux` agents on it: the backend's conformance suite, and real Claude across
a sleep; `vibe doctor` against a live `vibed`; and a release DMG, built ad
hoc with Sparkle and checked inside and out. It prints a
table of PASS, FAIL, N/A and SKIP, and leaves logs in `build/check/`. N/A means a step has
nothing it can check on this machine, such as no real server or real cgroups to use, and
isn't a pass. On Linux the macOS steps are skipped; on a Mac, a skipped step fails the run.

CI runs the same script on Linux x86_64, Linux arm64 and macOS 26, on merges to `main` or by
hand, and not on pull requests. Its macOS runner is a VM with no Docker, so it doesn't cover
what boots real VMs: `vm-cycle` is N/A there, `base-image` fails, and `linux-vm` is skipped
behind it. Those need a Mac on real hardware. `terminal-attach` has been failing on that
runner too, as Terminal doesn't answer Apple events there, so the macOS job has been red on
`main`. Steps that run real Claude take their
token from `CLAUDE_CODE_OAUTH_TOKEN`, and read it from the Keychain only with
`VIBE_CHECK_KEYCHAIN_TOKEN=1`.

A saved VM must also survive a reboot of the Mac. That takes a reboot, so
it's two commands around one: `scripts/vm-reboot-test.sh save`, reboot,
then `scripts/vm-reboot-test.sh restore`.

## Release

Push a tag `vX.Y.Z`, where `X.Y.Z` is the version in `Cargo.toml` and
`swift/VibeOS/Support/Info.plist` (both of its version keys; check-full.sh
checks they agree). `.github/workflows/release.yml` runs on version tags
only. It holds `Cargo.lock` to RustSec as it stands that day
(`scripts/advisories.sh`), and with an advisory open publishes nothing. Fix it,
or list it in `deny.toml` with its reason, on `main`, and tag that. It publishes
the Linux base image as release `base-<n>` if that isn't out yet, then builds the app with `scripts/release-macos.sh`: signed with
the Developer ID under the hardened runtime, notarized and stapled, in a
notarized DMG, with Sparkle and its appcast. It publishes those as release
`vX.Y.Z`, marked latest, where Sparkle looks for updates. It reads these
repository Actions secrets:

| Secret | What |
| --- | --- |
| `MACOS_CERT_P12`, `MACOS_CERT_PASSWORD` | The Developer ID Application certificate and key, as a base64 `.p12`, and its password |
| `APPLE_TEAM_ID` | The Developer ID's team |
| `NOTARY_KEY_ID`, `NOTARY_ISSUER_ID`, `NOTARY_KEY_P8` | An App Store Connect API key for notarization (the `.p8`'s text); or `APPLE_ID` and `APPLE_APP_PASSWORD` instead |
| `SPARKLE_PRIVATE_KEY` | Sparkle's EdDSA private key, as Sparkle's `generate_keys -x` exports it, or from `scripts/sparkle.py keygen`. The app's public key is derived from it |

Without them, the same script makes an ad hoc signed DMG for trying on this
Mac:

```sh
scripts/release-macos.sh               # build/release/out/VibeOS-<version>.dmg
```

Without `zig` and `cargo-zigbuild` on your PATH, the first run on a machine also
needs the network and a few more minutes, to get them for the Linux builds. Every
checkout shares them after that.

Only a tag on a commit that's on `main` releases.
