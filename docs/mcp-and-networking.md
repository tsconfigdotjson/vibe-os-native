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

### One MCP for every host

A Linux host's MCP reaches the whole hub, not only that host. While the Mac is reachable,
the same URL and key give you:

- that host's own agents
- the Mac's agents, as `mac:<ref>`
- agents on the Mac's other Linux hosts, by their names, such as `edge:<ref>`

The Mac keeps a private `ssh -R` tunnel to each linked host, so nothing is opened on the Mac
and nothing listens on a public address. The tunnel ends at a Unix socket in the host's
private `run/`. Calls for another host go through the Mac, so your Linux hosts don't need SSH
to each other.

The Mac checks every call that comes down a tunnel with its own key list. A key made on the
Mac works everywhere it's been pushed. A key made on a Linux host with `vibe key create` only
works for that host's own agents.

This needs `AllowStreamLocalForwarding` on the server's sshd, which is on by default.
**Settings → Hosts** says if a tunnel can't come up.

If more than one Mac links to the same host, they share its one tunnel end, so the host's
`mac` can move between them. Use the Mac's own MCP in that setup.

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
- Tailscale can't reach a Mac that's asleep or off. Its agents, and anything reached through
  it, are unavailable until it's back.

### When the Mac is off

A Linux host doesn't need the Mac to keep working. Its agents keep running, and its MCP
keeps answering for them:

- From the server itself, use `http://127.0.0.1:7878/mcp` with an API key.
- From elsewhere, use `ssh -t HOST vibe attach REF` for a terminal, or the host's
  Tailscale address if you've turned it on.
- On the server, `vibe project`, `vibe profile` and `vibe key` replace the
  Settings screens.

Everything that goes through the Mac, its own agents and those on its other hosts, shows as
unavailable, as last seen, until it's back. It's never reported as reachable while it isn't.
You can't start or send to those agents until then. It all merges back by itself when the
tunnel returns.

### Your own networking

SSH tunnels, VPNs and reverse proxies are yours to set up, and VibeOS doesn't
stand in their way. It also doesn't test or support them. Whatever you put in
front of MCP, keep API keys required and don't put it on the open internet
unprotected.

## If you used External Address

External Address served MCP publicly from a Linux host at `https://<domain>/mcp`, with a
Let's Encrypt certificate on port 443. That public listener is retired
([#143](https://github.com/tsconfigdotjson/vibe-os-native/issues/143)), and nothing replaces
it as a public path. The private path to the Mac stays, and is now on for every linked host:
see [One MCP for every host](#one-mcp-for-every-host).

> [!WARNING]
> A linked host that still serves External Address is **not** upgraded by itself. It goes on
> serving, and **Settings → Hosts** says it's waiting for you. Choosing **Update…** there, or
> running `vibe host upgrade NAME`, ends its `https://<domain>/mcp`. Move anything that uses
> that URL first.

**What changes:**

- An upgraded `vibed` no longer listens on port 443 or asks Let's Encrypt for a certificate.
- `vibe host external` and its Settings controls are gone. The tunnel no longer needs turning
  on, or a public name.

**What doesn't:** API keys, SSH hosts and their links, agents, `mac:<ref>`, loopback MCP and
the Tailscale option all carry on as they were.

**Moving a client:** give it the host's loopback URL if it runs on that server, or its
Tailscale URL if it runs elsewhere, with the same key. It keeps reaching the same agents.
Set up [Tailscale](#other-machines-over-tailscale) on the host first if you need that.

**Forgetting a host that may still serve it:** VibeOS refuses while the host's link is down
or it won't take your API keys off, because they'd go on working there, out of reach of a
revoke from the Mac. Once it's back, remove it again. If it's gone for good, choose **Remove
Anyway** or use `vibe host remove NAME --force`, and if it ever comes back, revoke those keys
on it with `vibe key revoke`.

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
| The Mac | The old tunnel socket | `rm "$HOME/Library/Application Support/VibeOS/run/tunnel.sock"*` |

If your API keys were used from the internet, consider replacing them.

**Going back.** A host's registry keeps its External Address setting, so the build before this
one can still read it, and that build **starts serving publicly again** if the setting was on.
To turn it off for good, run `vibe host external local off` on the host before you upgrade.

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
