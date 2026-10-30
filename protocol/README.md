# The `vibed` socket contract

`vibed` serves one JSON-RPC 2.0 API, and the CLI (`vibe`), the menu bar app
(`VibeOS.app`) and MCP all call it. This directory is the single source of
truth for that contract. Both implementations are tested against it:

| File | What it pins |
| --- | --- |
| `vibed-rpc.json` | Protocol version, framing, socket paths, the methods with their param and result types, the notifications the daemon sends, the error codes, the enums and their values, the exact field names of every type, which fields may be absent, and which fields hold other types (`nested`) |
| `examples/*.json` | Golden exchanges: a `request` (or a malformed `request_raw` line), the exact `response` (`null` for none) and any `events` the daemon sends after it; or a `notification` the daemon sends unasked |

The checks:

- **Rust, `vibe-core`** (`crates/vibe-core/tests/contract.rs`): the index matches
  `vibe_core::rpc` (version, methods, notifications, error codes, enums, each
  type's serde field names, optional fields), every example decodes into the
  typed structs and re-encodes to the same JSON, each field can be dropped
  exactly when it's optional, and the examples cover every method,
  notification, error and field, nested types included. The one error left
  out is `host_config_invalid`, which needs a broken `host.toml`;
  `crates/vibed/tests/core.rs` covers it.
- **Rust, `vibed`** (`crates/vibed/tests/contract.rs`): the examples replay in
  file order, as one session, through the daemon's dispatcher and over a real
  socket, giving exactly the recorded responses and events. `vibe rpc`
  replays them too (`crates/vibe-cli/tests/cli.rs`).
- **Swift, `VibeRPC`** (`swift/VibeOS/Tests/VibeRPCTests`): the index matches
  the Swift types' `CodingKeys`, method names, notifications, enums, error
  codes and the macOS socket path, every example round-trips through the
  Codable types, and each field can be dropped exactly when it's optional.
  With `VIBE_TEST_SOCKET` set, it also calls a live `vibed`. A few Swift names
  differ from the index's, to stay clear of SwiftUI and Foundation
  (`AgentEnvironment` for `Environment`, `DaemonEvent` for `Event`, …); the
  tests map them.

To change the contract, edit this directory first, then both sides, and bump
`protocol_version` for anything a running peer would misread.

**Framing.** One JSON value per line, UTF-8, `\n`-terminated, in both
directions: on the Unix socket, and through `vibe rpc` (stdin/stdout) over
SSH. A line longer than `max_message_bytes` gets an error and the connection
is closed. Batches are supported, and run in order. Params are always by
name; a method that takes none accepts `{}` or no params at all.

**The examples are one session.** From `20-` on they run in file order
against one daemon: a Mac (`os` `macos`, so new projects get two starter
profiles) whose home is `/Users/me`, at 2026-10-30T12:00:00Z, with `src/app`
checked out and `me/web` on GitHub. Its sessions are `quiet-amber-otter`,
a real `vibe-shim` running `sleep`, and the records of two that exited:
`brave-copper-lynx` with code 0 and `calm-silver-heron` killed. The Rust
replays build that world in a temp directory and put its path in place of
`/Users/me`.

**Shims aren't in this contract.** Each session's `vibe-shim` serves
`run/<ref>.sock` with a framed protocol of its own (`vibe_core::shim`),
which only `vibe attach` and `vibed` speak. Clients find a session with
`sessions.attach` here.

**Hosts.** Every method that acts on one host takes an optional `host`;
absent means this one, which every `vibed` calls `local`. Other hosts come
with Linux hosts (#16).

**Units.** Sizes are bytes and durations are seconds, as integers. Only
`host.toml` writes them as `"4G"` and `"60s"`. Times are RFC 3339 in UTC.

**Hello.** `daemon.hello` is how a client checks it speaks the daemon's
protocol version. Clients send it first on a connection they keep; the
daemon doesn't require it, so one-call clients (`vibe hello`, the Swift
`VibedClient`, which opens a connection per call) can make a call directly.

**Events.** After `events.subscribe` answers, that connection also gets an
`events.event` notification (no `id`) for every event, in `seq` order,
interleaved with the responses to its own calls. An event a call causes may
arrive before that call's response, so match responses by `id`, or better,
use a connection of its own for events. A subscriber that falls too far
behind gets a `lagged` event in place of what it missed, carrying the `seq`
of the last event it missed, and should read everything again. `seq`s only
go up. When the client closes
its end, events already due to it, such as those its last call caused, are
sent before the daemon closes; a client that has stopped reading gets 10 s
for that.
