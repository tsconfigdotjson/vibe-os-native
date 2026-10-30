# The `vibed` socket contract

`vibed` serves one JSON-RPC 2.0 API, and the CLI (`vibe`), the menu bar app
(`VibeOS.app`) and MCP all call it. This directory is the single source of
truth for that contract. Both implementations are tested against it:

| File | What it pins |
| --- | --- |
| `vibed-rpc.json` | Protocol version, framing, socket paths, the methods with their param and result types, the error codes, the exact field names of every type, and which fields may be absent |
| `examples/*.json` | Golden exchanges: a `request` (or a malformed `request_raw` line) and the exact `response`, or `null` for none |

The checks:

- **Rust, `vibe-core`** (`crates/vibe-core/tests/contract.rs`): the index matches
  `vibe_core::rpc` (version, methods, error codes, each type's serde field
  names, optional fields), every example decodes into the typed structs and
  re-encodes to the same JSON, each field can be dropped exactly when it's
  optional, and the examples cover every method, error and field.
- **Rust, `vibed`** (`crates/vibed/tests/contract.rs`): every example replays
  through the daemon's dispatcher, and over a real socket, giving exactly the
  recorded response.
- **Swift, `VibeRPC`** (`swift/VibeOS/Tests/VibeRPCTests`): the index matches
  the Swift types' `CodingKeys`, method names, error codes and the macOS
  socket path, every example round-trips through the Codable types, and each
  field can be dropped exactly when it's optional. With
  `VIBE_TEST_SOCKET` set, it also calls a live `vibed`.

To change the contract, edit this directory first, then both sides, and bump
`protocol_version` for anything a running peer would misread.

**Framing.** One JSON value per line, UTF-8, `\n`-terminated, in both
directions: on the Unix socket, and through `vibe rpc` (stdin/stdout) over
SSH. A line longer than `max_message_bytes` gets an error and the connection
is closed. Batches are supported. Params are always by name.
