# MCP and networking

MCP is how you drive VibeOS from other tools. This page covers where the server
listens, how clients sign in, and how to reach it from another machine. For the
full design, see [PRD.md](../PRD.md).

## API keys

Every MCP call needs an API key. There's no unauthenticated mode, not even on
loopback.

Make a key in **Settings → API Keys** on the Mac, or with `vibe key create NAME`
on a Linux host. The key is shown once. Clients send it as
`Authorization: Bearer <key>`. Settings gives you a ready-made setup line:

```sh
claude mcp add --transport http vibeos http://127.0.0.1:7878/mcp --header "Authorization: Bearer vk_…"
```

For clients that can only start a command, `vibe mcp` bridges stdio to that
endpoint, reading the key from `VIBE_API_KEY`.

> [!WARNING]
> A key has full access, with no scopes. It's equivalent to a shell on the host,
> so keep it like a password and revoke it if it leaks. Keep keys out of shared
> shell history, logs and chat.

Keys made on the Mac are pushed to linked Linux hosts, so one key works on both.
A host that's offline during a revoke keeps accepting that key until it's back,
and Settings → API Keys names any host that's behind.

## Where MCP listens

| Host | Always | Optional |
| --- | --- | --- |
| Mac | `127.0.0.1:7878` | Its Tailscale address, from Settings |
| Linux host | `127.0.0.1:7878` | Its Tailscale address, from `host.toml` |

The port is `mcp_port` in `~/.config/vibeos/host.toml`. These listeners never
bind `0.0.0.0`, and the Tailscale option only ever binds addresses in Tailscale's
ranges.

## Ways to connect

### On the same machine

Use the loopback URL with an API key. A Mac's MCP lists and drives agents on
that Mac and on every Linux host linked to it.

### Linux servers, over SSH

**Settings → Hosts → Add Linux Host…** (or `vibe host add NAME SSH-DESTINATION`)
takes any SSH destination and needs key-based login to already work. Tailscale
isn't required. If a server happens to be on your tailnet, its Tailscale name
works as the destination too.

VibeOS installs `vibed` on the server as a systemd user service and links it to
the Mac over SSH, so nothing new is opened on the server. Its agents then appear
in the Mac's menu and MCP. Clicking one attaches with `ssh -t HOST vibe attach
REF`.

### Other machines, over Tailscale

If you want a laptop or another machine to reach MCP, we suggest
[Tailscale](https://tailscale.com). It's entirely optional.

- **On the Mac:** turn on **Settings → Hosts → Also serve MCP on this Mac's
  Tailscale address**, or **Turn On** in the welcome. Then use the Tailscale URL
  Settings shows, with an API key.
- **On a Linux host:** set `mcp_tailscale = true` in its
  `~/.config/vibeos/host.toml`. VibeOS picks the change up within seconds. This
  is a file setting, as Settings only offers the toggle for the Mac. `vibe key
  create` on the host prints the tailnet setup line once it's serving there.

What to expect:

- VibeOS doesn't install, sign in to or configure Tailscale. Both machines need
  it running and logged in to the same tailnet, and your tailnet's access rules
  must allow the port. `vibe doctor` reports whether this host has a Tailscale
  address, but not whether another machine can reach it.
- The URL is plain HTTP inside your tailnet, which Tailscale encrypts. API keys
  are still required.
- It only reaches your own tailnet. It isn't a way to open MCP to the internet.
- Tailscale can't reach a Mac that's asleep or off. Its agents are unavailable
  until it's back.

### When the Mac is off

A Linux host doesn't need the Mac to keep working. Its agents keep running, and
it serves its own MCP for its own agents:

- From the server itself, use `http://127.0.0.1:7878/mcp` with an API key.
- From elsewhere, use `ssh -t HOST vibe attach REF` for a terminal, or the host's
  Tailscale address if you've turned it on.
- On the server, `vibe project`, `vibe profile` and `vibe key` replace the
  Settings screens.

The Mac's own agents are unavailable until it's back.

### Your own networking

SSH tunnels, VPNs and reverse proxies are yours to set up, and VibeOS doesn't
stand in their way. It also doesn't test or support them. Whatever you put in
front of MCP, keep API keys required and don't put it on the open internet
unprotected.

## If you used External Address

External Address served MCP publicly from a Linux host at `https://<domain>/mcp`,
with a Let's Encrypt certificate on port 443 and an `ssh -R` tunnel back to the Mac.
It's retired ([#143](https://github.com/tsconfigdotjson/vibe-os-native/issues/143)),
and nothing replaces it as a public path.

> [!WARNING]
> Upgrading ends it, with no prompt. A Mac upgrades its linked hosts by itself, and a
> host's `https://<domain>/mcp` stops answering as soon as its `vibed` is the new one.
> Move anything that uses that URL first.

**What changes:**

- `vibed` no longer listens on port 443, asks Let's Encrypt for a certificate or
  keeps an `ssh -R` tunnel. The old tunnel ends with the old `vibed` on the Mac.
- A client on another machine can no longer reach the Mac's agents through a Linux
  host. Agents named `mac:<ref>` are gone from a host's MCP.
- `vibe host external` and the Settings controls are gone.

**What doesn't:** API keys, SSH hosts and their links, agents, loopback MCP and the
Tailscale option all carry on as they were.

**Moving a client:** use one of the [ways to connect](#ways-to-connect) above. Tailscale
is the closest match for a client on another machine. For a client on the server
itself, use its loopback URL.

**Before you forget a host:** let it upgrade first, or run `vibe host external local off`
on it. A host that's still on an older build, with its link down, can go on serving
publicly with your API keys, where a revoke from the Mac can't reach it.

**Clearing up.** `vibed` deletes nothing. If it finds External Address settings or
certificates, it logs a note at each start. What's left is inert, and these are yours to
remove once the new build has settled:

| On | What | How |
| --- | --- | --- |
| Linux host | The certificate, its private key and the ACME account key | `rm -r ~/.local/share/vibeos/external` |
| Linux host | The helper's unprivileged copy | `rm ~/.local/lib/vibeos/vibe-bind` |
| Linux host, if the helper was installed | The root-owned helper, which can open port 443 | `sudo rm /usr/local/lib/vibeos/vibe-bind-1 && sudo rmdir /usr/local/lib/vibeos` |
| Linux host, if the fallback was used | A setting that lets any user there open ports from 443 up | `sudo rm /etc/sysctl.d/60-vibeos.conf && sudo sysctl -w net.ipv4.ip_unprivileged_port_start=1024` |
| Your DNS and firewall | The name pointing at the host, and port 443 open to it | Remove them if nothing else uses them |

Any `mac.sock` or `tunnel.sock` left in `run/` does no harm, and you can delete it. If your
API keys were used from the internet, consider replacing them.

**Going back.** The registry keeps its External Address settings, so the build before
this one can still read it, and that build **starts serving publicly again** if the
setting was on. To turn it off for good, run `vibe host external local off` on the host
before you upgrade.

## MCP tools

| Tool | Does |
| --- | --- |
| `vibe_list_projects` | Lists projects on every connected host, with their profiles |
| `vibe_start_agent` | Creates a worktree, branch, compartment and agent from a project, profile and prompt |
| `vibe_list_agents` | Lists agents with their host, state, branch, memory and attach command |
| `vibe_wait` | Blocks until an agent answers the last send |
| `vibe_read_messages` | Reads an agent's last messages from its transcript |
| `vibe_read_output` | Reads the agent's screen and recent scrollback |
| `vibe_send` | Sends text or keys, waking the agent first if it's asleep |
| `vibe_delete_agent` | Ends an agent and removes what VibeOS made for it, keeping the branch. Refuses on unsaved work unless forced |
