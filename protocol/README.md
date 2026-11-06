# The `vibed` socket contract

`vibed` serves one JSON-RPC 2.0 API, and the CLI (`vibe`), the menu bar app
(`VibeOS.app`) and MCP all call it. This directory is the single source of
truth for that contract. Both implementations are tested against it:

| File | What it pins |
| --- | --- |
| `vibed-rpc.json` | Protocol version, framing, socket paths, the methods with their param and result types, the notifications the daemon sends, the error codes, the enums and their values, the exact field names of every type, which fields may be absent, and which fields hold other types (`nested`) |
| `pi-models.json` | What a Pi profile's model may be: names accepted and refused. vibed's `validate_pi_model` and Swift's `isValidPiModel` each test against this list |
| `examples/*.json` | Golden exchanges: a `request` (or a malformed `request_raw` line), the exact `response` (`null` for none) and any `events` the daemon sends after it; or a `notification` the daemon sends unasked |
| `examples/frames/call-accepted.json` | The `call.accepted` frame a peer sends ahead of an answer, beside the call it's for: apart, as it's not part of the examples' session |

The checks:

- **Rust, `vibe-core`** (`crates/vibe-core/tests/contract.rs`): the index matches
  `vibe_core::rpc` (version, methods, notifications, error codes, enums, each
  type's serde field names, optional fields), every example decodes into the
  typed structs and re-encodes to the same JSON, each field can be dropped
  exactly when it's optional, and the examples cover every method,
  notification, error and field, nested types included. The one error left
  out is `host_config_invalid`, which needs a broken `host.toml`;
  `crates/vibed/tests/core.rs` covers it. When the contract changes, the
  ignored test `record_examples` in `crates/vibed/tests/contract.rs`
  writes what the daemon answers each example with, to review and copy in.
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
  (`AgentEnvironment` for `Environment`, `AgentActivity` for `Activity`,
  `DaemonEvent` for `Event`, …); the tests map them.

To change the contract, edit this directory first, then both sides, and bump
`protocol_version` for anything a running peer would misread.

**Pi.** The last examples, `99pi-*`, are Pi's: the golden home has a login each for the owner and for `linux` agents (fakes), and a release to install from, and its backend runs the stand-in for Pi, which also answers `pi.models` and `pi.login.*` as `vibe-setup.ts` makes the real one do. Randomness is fixed, so a sign-in's id is too. A Pi agent refused at its start for want of a login carries `{"pi_sign_in": {"provider": <name or null>, "target": "native" | "vm"}}` in its error's `data`, for a client to offer that sign-in.

**Framing.** One JSON value per line, UTF-8, `\n`-terminated, in both
directions: on the Unix socket, and through `vibe rpc` (stdin/stdout) over
SSH. A line longer than `max_message_bytes` gets an error and the connection
is closed. Batches are supported, and run in order. Params are always by
name; a method that takes none accepts `{}` or no params at all.

**The examples are one session.** From `20-` on they run in file order
against one daemon: a Mac (`os` `macos`, so new projects get two starter
profiles) whose home is `/Users/me` and Tailscale address `100.101.102.103`, at
2026-10-30T12:00:00Z, with `src/app`
checked out and `me/web` on GitHub. Its sessions are `quiet-amber-otter`,
a real `vibe-shim` running `sleep`, and the records of two that exited:
`brave-copper-lynx` with code 0 and `calm-silver-heron` killed. `src/app`
has an `origin` to fetch, and a `.vibe/setup.sh` that copies `.env.example`
to `.env.local`. Agents run under `vibe-env`'s test backend, with its
stand-in for Claude Code, on a host with 64G of memory, 58G of it in use and
3G of its 4G of swap, while macOS warns of memory pressure; randomness is fixed,
so refs and session ids are too. Its toolbox is `vibe-env`'s test toolbox
(a shell of `sleep 600`, empty images for versions), with base release 1
installed, a Linux home (version 1, holding a `gh` login and a git
identity; linux agents have no Claude login yet) and a `toolbox.toml` whose
`[update]` runs `true` and whose one `[[install]]` fails the first time it's
run. `dark-mode-review`'s transcript has a turn in it already, so its
conversation reads the same every time. Every Linux host it adds is an
x86_64 server called `vps-1` without `claude`, and no link comes up. Its
`vibe doctor` finds git 2.50.1, `gh` logged in, git not yet using it, Claude
Code 2.1.314 signed in, a Mac that sleeps after 10 minutes, and the Linux
images on their way: Claude Code's layer in, base release 2 40% downloaded.
Its login to Linear needs reconnecting, as the stand-in for Linear has forgotten
its client, and has no tools, so nothing it does in the background adds events.
The
Rust replays build that world in a temp directory and put its path in place
of `/Users/me`.

**`vibe-vm` has a contract of its own**, in `vibe-vm/`: the configuration
`vibed` gives it, the line it prints as it starts, and its control socket.
See [vibe-vm/README.md](vibe-vm/README.md).

**Shims aren't in this contract.** Each session's `vibe-shim` serves
`run/<ref>.sock` with a framed protocol of its own (`vibe_core::shim`),
which only `vibe attach` and `vibed` speak. Clients find a session with
`sessions.attach` here.

**Hosts.** Every method that acts on one host takes an optional `host`;
absent means this one, which every `vibed` calls `local`. A Linux host is
added with `hosts.add` (an install over SSH) and goes by the name it was
added under, such as `vps`. A call that names it goes to that host's own
`vibed` over its link, `ssh <host> vibe rpc`, which carries this same
contract, and its answer comes back with the host's name in place of its
`local` (an agent's `attach` becomes the `ssh -t …` line that reaches it).
While the link is down such a call fails with `host_unavailable`, except
`agents.list`, which gives the host's agents as they were last listed.
`hosts.list` gives each host's `link` (`local` for itself; `connected` or
`down` for a Linux host, with `detail` saying why it's down), the `vibed`
it runs, what it's `missing` of `git`, `gh` and `claude`, and whether it serves
MCP now, on loopback at least, for the menu's header. `external` is retired: it's
always `false`, and only sent for an older Mac that can't read a host's row
without it. `mcp.changed` says when that changes, and
`host.added`, `host.removed` and `host.link` when the hosts or their links
do. A Linux host's events come onto the Mac's stream under its name. The Mac
pushes its API keys to each linked host by their hashes (`keys.sync`), so
one key works on every host. It says which Mac it is (`from`), and replaces only
what that Mac pushed, so a host can be shared.

**Calls over a link.** A connection to `vibed.sock` whose hello says it's a link gets
`accepts_calls: true` in its `daemon.hello` answer. For the agent calls MCP's tools
stand on (`hosts.list`, `projects.list`, `agents.list`, `agents.start`,
`agents.delete`, `agents.send`, `agents.check`, `agents.output`,
`agents.messages`), it's then told as soon as a call is taken, with a notification,
`call.accepted {id}`. The Mac fails one that hasn't that or an answer within 5 s,
however long it may run after that.

**Shared MCP services.** There's one login per install for each service, held by
one owning host, and a broker on every `vibed`. `services.overview` gives each
service's one connection: who owns it, and how every host stands with it.
`services.list` and `services.set` take a `host`, and go to it over its link, as
each host has its own settings. `services.connect` goes to the host that's to own
the login: it registers the client and makes the PKCE verifier, which it keeps, and
answers with the page for the app's web authentication session to open.
`services.complete` takes the URL that session ended on, and that host makes the
exchange, which makes it the owner; any host that was is told to forget its login.
Only a code that's useless without the verifier crosses the link for that, and the
refresh token never does. `services.disconnect` forgets the login wherever it is.
`services.token` and `services.follow` are how the Mac carries an access token from
the owning host to each other host. They go only between `vibed`s, and the app never
calls them: anything else asking gets `unauthorized` (-32006).
`services.audit` gives the last calls agents on that host made to its services,
which stay there. If `services.set` can't keep a setting, nothing changes and it
says so. `service.changed` says when a service's state, settings or tools change:
`state` is its state and `message` its name, and a Linux host's comes onto the
Mac's stream under its name. `ServiceInfo` holds nothing secret.
In the examples, `http://services.example` is a stand-in for the service, on a
port of its own; the Rust replays put its address in its place.

**The shared services' own socket isn't this contract.** Every host's
`run/.services.sock` is what `vibe mcp-proxy` reaches: one JSON line each way,
then MCP's JSON-RPC, one message per line (`vibe_relay::local`).

The methods MCP's `vibe_send`, `vibe_wait`, `vibe_read_output` and
`vibe_read_messages` stand on are here too, as `agents.send`,
`agents.check`, `agents.output` and `agents.messages`, so the Mac can reach
a Linux host's agents over its link.

**Units.** Sizes are bytes and durations are seconds, as integers. Only
`host.toml` writes them as `"4G"` and `"60s"`. Times are RFC 3339 in UTC.

**Hello.** `daemon.hello` is how a client checks it speaks the daemon's
protocol version. Clients send it first on a connection they keep; the
daemon doesn't require it, so one-call clients (`vibe hello`, the Swift
`VibedClient`, which opens a connection per call) can make a call directly.

**Events.** After `events.subscribe` answers, that connection also gets an
`events.event` notification (no `id`) for every event, in `seq` order,
interleaved with the responses to its own calls: a call's response comes
after the events published before the call, and before those published
while it ran, such as those it caused. Still, match responses by `id`, or
better, use a connection of its own for events. A subscriber that falls too far
behind gets a `lagged` event in place of what it missed, carrying the `seq`
of the last event it missed, and should read everything again. `seq`s only
go up. When the client closes
its end, events already due to it, such as those its last call caused, are
sent before the daemon closes; a client that has stopped reading gets 10 s
for that.
