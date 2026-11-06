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
| Linux host | `127.0.0.1:7878` | Its Tailscale address, and `https://<machine>.<tailnet>.ts.net/mcp` through Tailscale Serve, from Settings or by hand |

The port is `mcp_port` in `~/.config/vibeos/host.toml`. These listeners never
bind `0.0.0.0`, and the Tailscale option only ever binds addresses in Tailscale's
ranges. Tailscale Serve is Tailscale's, and answers on your tailnet only.

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
in the Mac's menu and MCP. Clicking one attaches with `ssh -t HOST
.local/lib/vibeos/vibe attach REF`, by that path because a command over SSH doesn't
get a login shell's `PATH`.

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

- **On the Mac:** turn on **Settings → Hosts → Serve MCP on this Mac's
  Tailscale address**, or **Turn On** in the welcome. Then use the Tailscale URL
  Settings shows, with an API key.
- **On a Linux host:** turn on **Settings → Hosts → Serve MCP on HOST's Tailscale
  address**. You don't need SSH. Tailscale must already be installed and logged in on the
  host. Settings then shows `https://<machine>.<tailnet>.ts.net/mcp` and adds it to the
  **Copy Setup Command** menu when you make a key. `vibe key create` on the host prints it
  too.

### The Linux host toggle

**Turning it on** runs this on that host, then sets `mcp_tailscale = true` there:

```sh
tailscale serve --bg --https=443 --set-path=/mcp http://<tailscale-ip>:<mcp_port>/mcp
```

Serve must point at the Tailscale listener. Pointing it at `127.0.0.1` gets calls rejected,
because the loopback listener only accepts loopback `Host` headers and Serve passes the
`ts.net` name.

**Turning it off** runs `tailscale serve --https=443 --set-path=/mcp off` and sets
`mcp_tailscale = false`. Anything else you serve stays as it is, including paths under
`/mcp/`.

**When it stops and changes nothing:**

| What Settings says | Why | What to do |
| --- | --- | --- |
| Tailscale already serves `/mcp` from something else | `/mcp` points anywhere but this host's listener, including `127.0.0.1` or a port it's no longer on, or anything at all is at `/mcp/` or in a foreground `tailscale serve`. VibeOS won't overwrite it or remove it | If nothing needs it, run the `tailscale serve … off` line Settings gives on the host, or stop the `tailscale serve` it says is running in the foreground, then try again |
| Funnel is on for that name | `/mcp` there would be on the public internet. VibeOS never turns Funnel on, and won't turn yours off | Turn Funnel off for that name yourself, then try again. `tailscale funnel status` on the host shows what it exposes |
| Tailscale doesn't let you change what it serves | The host's user isn't Tailscale's operator | Choose **Allow…** on the host's row, or run `sudo tailscale set --operator=<user>` on the host. Then turn the toggle on again |
| Your tailnet needs HTTPS turned on | HTTPS or Serve is off for your tailnet | Turn HTTPS on for your tailnet, and try again. If `tailscale` printed a link on the host, Settings shows it |
| Tailscale isn't installed, logged in or running | VibeOS leaves those to you | Set it up on the host. The toggle is unavailable until then |
| Tailscale has no name for this host | The `ts.net` name comes from MagicDNS | Turn on MagicDNS for your tailnet. The toggle is unavailable until then |

If Funnel is turned on later for a name that already serves `/mcp`, MCP is on the public
internet behind its API keys. Settings warns you, and you can still turn the toggle off.

**On a host with no Mac,** set it up by hand: set `mcp_tailscale = true` in
`~/.config/vibeos/host.toml`, which VibeOS picks up within seconds, and run the
`tailscale serve` line above. The toggle recognizes the result as on. With the file setting
alone, MCP is at `http://<tailscale-ip>:<mcp_port>/mcp`, and `vibe key create` prints that line.

### Reaching MCP over Tailscale

What to expect:

- VibeOS doesn't install or sign in to Tailscale. All it configures is the `/mcp` path on
  a Linux host's Serve, when you turn that toggle on, and Tailscale's operator there, if you
  choose **Allow…** and enter your password. Both machines need
  Tailscale running and logged in to the same tailnet, and your tailnet's access rules
  must allow the port. `vibe doctor` reports whether this host has a Tailscale
  address, but not whether another machine can reach it.
- A `http://<tailscale-ip>:…` URL is plain HTTP inside your tailnet, which Tailscale
  encrypts. A `https://….ts.net/mcp` URL also has a certificate. API keys are required on both.
- It only reaches your own tailnet. It isn't a way to open MCP to the internet.
- Tailscale can't reach a Mac that's asleep or off. Its agents, and anything reached through
  it, are unavailable until it's back.

### When the Mac is off

A Linux host doesn't need the Mac to keep working. Its agents keep running, and its MCP
keeps answering for them:

- From the server itself, use `http://127.0.0.1:7878/mcp` with an API key.
- From elsewhere, use `ssh -t HOST .local/lib/vibeos/vibe attach REF` for a terminal,
  which is where Add Linux Host installs `vibe`, or the host's Tailscale URL if you've
  turned it on. Serve and `mcp_tailscale` are both kept on the host, so that URL doesn't
  depend on the Mac.
- On the server, `vibe project`, `vibe profile` and `vibe key` replace the
  Settings screens.

Everything that goes through the Mac, its own agents and those on its other hosts, shows as
unavailable, as last seen, until it's back. It's never reported as reachable while it isn't.
You can't start or send to those agents until then. It all merges back by itself when the
tunnel returns. Last-seen agents are kept in memory, so a host restarted while the Mac is
away shows the Mac as unavailable, with none of its agents, until it's back.

### Your own networking

SSH tunnels, VPNs and reverse proxies are yours to set up, and VibeOS doesn't
stand in their way. It also doesn't test or support them. Whatever you put in
front of MCP, keep API keys required and don't put it on the open internet
unprotected.

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
