# The `vibe-guest` contract

`vibe-guest` is PID 1 inside every `linux` agent's VM (PRD: What's inside
`linux`). `vibed` drives it through `vibe_env::guest`, and `vibe-shim --vm`
holds its terminal; the types are `vibe_guest::proto` and
`vibe_guest::boot`, which both sides share. This directory pins what passes
between them, and `crates/vibe-guest/tests/contract.rs` holds the Rust types
to it:

| File | What it pins |
| --- | --- |
| `cmdline.txt` | The kernel command line: what to stack and mount, and who `vibe` is |
| `cmdline-toolbox.txt` | The toolbox's: the template and the Linux home on writable disks of their own, and `sudo` |
| `examples/*.json` | Requests on the control port with their exact replies (`request`, or a malformed `request_raw`, and `reply`) |
| `terminal/*.json` | The JSON frames on the terminal port (`kind` and `payload`) |

Only `vibe-vm` can reach a VM's vsock, so the host reaches its ports with
`vibe-vm`'s `connect` (`protocol/vibe-vm/`). The guest's real test is
`crates/vibe-env/tests/vm_cycle.rs` on a Mac: `guest_question_survives_sleep`
boots it, starts a stand-in for Claude with a question pending, and puts it
to sleep and wakes it through a real `vibe-shim`.

## Boot: the kernel command line

`vibed` writes the VM's `cmdline` in `vm.json`, and appends
`vibe_guest::boot::Boot::to_cmdline` to it. The guest reads `/proc/cmdline`;
every word it reads starts `vibe.`, which the kernel keeps out of PID 1's
arguments and environment.

| Word | Means |
| --- | --- |
| `vibe.layers=managed,home,template,base` | The read-only disks, by `vm.json` id (their virtio serial), top first, stacked with overlayfs into the root. Absent: run from the initrd |
| `vibe.upper=agent` | The agent's writable disk, which takes every write to the root. Blank, it's formatted ext4 on the first boot. Absent with layers: writes go to memory |
| `vibe.home=home` | A second writable disk, for `/home/vibe` alone: its own overlay puts the home's writes in the disk's `upper/home/vibe`, so the disk can later be stacked as a layer of its own. The toolbox's Linux home; needs `vibe.upper` |
| `vibe.sudo=1` | `vibe` may run anything as root with `sudo`, through `/etc/sudoers.d/vibe-toolbox`: the toolbox only. Without it, a boot removes that file if a layer has one |
| `vibe.share=TAG:PATH` | Mount the virtio-fs share `TAG` at `PATH`, percent-encoded. The worktree and `.git` go at their host paths; the agent's Claude home at `/home/vibe/.claude`; the package caches where the tools look (`/home/vibe/.npm`, …) |
| `vibe.uid=501 vibe.gid=20` | Who `vibe` is: the host user, since virtio-fs shows the shares' files with their host owner. Absent: 1000. `vibe` gets entries in `/etc/passwd`, `/etc/group` and `/etc/shadow` (a password nobody can type, no ageing), which PAM's account check needs for `sudo` |

A layer disk is ext4 (or squashfs or erofs): either a whole root filesystem,
as the base image is, or a writable disk from an earlier VM, whose changes
are in its `upper/` directory, which is then the layer. That's how the
toolbox saves the template (its `vibe.upper`) and the Linux home (its
`vibe.home`). A word the guest can't read
doesn't stop the boot: it's a problem in the status, and a stack that makes
no sense isn't stacked at all.

A writable disk is formatted only if its first MiB is all zeros, as a new
sparse image is; one with data but no filesystem the guest knows is left
alone, and the guest runs from its initrd and reports it.

The root gets `/proc`, `/sys`, `/dev`, `/dev/pts`, `/dev/shm`, `/run` and
`/sys/fs/cgroup` (cgroup v2); `/tmp` stays on the agent's disk. As PID 1,
`vibe-guest` ignores any arguments the kernel passes it.

What the image the guest boots must have (#9), since `vibe-guest` loads no
modules:

- an initrd with `vibe-guest` as `/init`, a static `mke2fs` on its `PATH`
  (`/sbin`), and `/dev/console` and `/dev/null` device nodes, so PID 1
  starts with its standard descriptors open
- a kernel with these built in: virtio PCI, block, net, console, balloon and
  entropy; vsock with its virtio transport; FUSE and virtio-fs; overlayfs;
  ext4; packet sockets (DHCP); cgroup v2 with the freezer (5.2 or later,
  and `cgroup.kill`, 5.14, to clear out what a command left behind)

The test guest is Alpine's netboot kernel, whose drivers its `/init` loads
first, and runs from its initrd, without layers.

## The control port: 1100

All its ports take connections from the host (vsock CID 2) only, never from a
process in the guest.

A connection carries one request line, sent within 30 s, and gets one reply
line, both JSON: `{"ok":true, …}` or `{"ok":false,"code":…,"error":…}`.

| Request | Does | Reply |
| --- | --- | --- |
| `{"op":"status"}` | | `protocol`, `version`, `boot_id` (new on every boot, kept by a restore), `uptime_ms`, `state` (`idle`, `running`, `exited`), `frozen`, the `session` (pid, command, cwd, `started_ms`, its `exit`), `memory` (`total`, `available`), the `network` once DHCP answers, the `layers` and `shares`, and `problems` |
| `{"op":"launch","command":[…],"env":{…},"cwd":…,"size":{…},"time_ms":…}` | Starts the command as `vibe` on a new terminal, in its own session and in the agent's cgroup. `env` is added to the guest's own (`HOME`, `PATH` with `/opt/vibeos/bin` first, `XDG_RUNTIME_DIR=/run/user/<uid>`, `TERM`, …): it's where the Claude token goes, and the guest never writes it down or logs it. `time_ms` sets the clock first. Whatever the last command left in the cgroup is killed | `pid`. `busy` while one runs or the agent is frozen |
| `{"op":"processes"}` | | Every process in the agent's cgroup, however started: `pid`, `ppid`, `name`, `args`, `state`, `started_ms`, `cpu_ms`, `rss`; and the guest's `uptime_ms` |
| `{"op":"freeze"}` | Freezes the cgroup (the cgroup v2 freezer, which the command can't see), then `sync`s every disk and share, so the host has the transcript whole while the agent sleeps. Before `vibe-vm`'s `sleep`. If the freeze doesn't settle in 5 s, the agent is let run again and the reply is `freeze_failed` | How many processes, how long each step took, and `memory`, for the balloon's floor (`vibe_env::guest::balloon_floor`) |
| `{"op":"thaw","time_ms":…,"share_timeout_ms":…}` | After a restore: sets the clock (it stopped while the VM slept), waits for every share to answer (about 0.9 s after a restore; 10 s at most by default), then thaws, whatever the clock and shares did, and renews the DHCP lease | `clock_step_ms`, `shares_ms`, `unanswered` shares, `thaw_ms` and `problems`; if the thaw itself fails, `freeze_failed`, with what the clock and shares did in the error |
| `{"op":"clock","time_ms":…}` | Sets the clock. A VM has no clock of its own to boot with | `step_ms` |
| `{"op":"run","command":[…],"env":{…},"cwd":…,"timeout_ms":…}` | Runs the command to completion as `vibe`, beside the agent's (Claude running or not): for `.vibe/setup.sh`. In a process group of its own, not the agent's cgroup, so a freeze doesn't stop it nor a launch clear it out. Past `timeout_ms` the group is killed, and what the command leaves in it is killed when it ends. The connection waits for it | `code` or `signal`, `timed_out`, and its stdout and stderr together as `output`: the last 32 KiB, as UTF-8 |
| `{"op":"poweroff"}` | Replies, ends the agent (`SIGTERM`, then after 3 s `SIGKILL`), syncs and powers off; `vibe-vm` then exits 0 | `{"ok":true}` |
| `{"op":"seal"}` | Before a poweroff whose writable disks are kept (the toolbox's): ends the command, takes `sudo` away, empties `/tmp`, syncs, then freezes every writable disk's filesystem (`FIFREEZE`), which empties its journal and marks it clean, so it can be stacked read-only later. After it, `launch` and `run` are refused with `busy`; sealing again gives the same reply | `disks` frozen, by id, and `seal_ms`; `seal_failed` if one couldn't be, and then nothing may be kept |

A share answers when its root lists and every name in it can be looked up;
before probing, the guest drops the dentries nobody holds, so lookups go to
the host afresh.

## The terminal port: 1101

Frames as on a shim's socket (`vibe_core::shim`): a kind byte, a big-endian
`u32` length, the payload, at most 1 MiB. Terminal bytes travel as they are;
the rest is JSON.

| Kind | Byte | Way | Payload |
| --- | --- | --- | --- |
| `hello` | 1 | shim → guest, first | `protocol`, `client`, and optionally `size`, applied before the screen is sent |
| `welcome` | 2 | guest → shim | `protocol`, the `session` and the terminal's `size`; then the screen as one `output` |
| `input` | 3 | shim → guest | bytes, as typed |
| `output` | 4 | guest → shim | bytes the terminal wrote |
| `resize` | 5 | shim → guest | `rows` and `cols` |
| `exit` | 8 | guest → shim | the command ended: `code`, or `signal` by name (`SIGKILL`; numbers differ between Linux and macOS). The guest then closes |
| `refused` | 9 | guest → shim, instead of a welcome | `message` and the guest's `protocol`; then it closes |

One shim at a time: a new connection takes over, and the last one is closed.
The guest keeps its own screen model and reads the terminal whether or not a
shim is connected, so the command never blocks on a missing one, and a shim
that connects after a dropped link, a crash or a restore is sent the screen
as it is. A shim that takes more than 5 s to take a frame is dropped. When the command has
already exited, a shim that connects gets the welcome, the last screen and
the exit at once.

## The shared services' relay: 1102

The Mac dials this port while the VM runs, for an agent whose profile has shared
MCP services on, and carries every shim in the VM to the Mac's broker over that
one connection. A newer connection replaces an older one, whose streams end. The
guest pings every 15 s and takes the Mac for gone after 45 s of silence. Its types
are `vibe_relay::frame`.

A frame is a kind byte, a big-endian `u32` stream, a big-endian `u32` length, and
that many bytes, at most 16 MiB.

| Kind | Byte | Way | Payload |
| --- | --- | --- | --- |
| `hello` | 0 | both, first | JSON: `protocol` |
| `open` | 1 | guest → Mac | JSON: `service`, and `agent` if the shim said. The Mac goes by which VM it dialed |
| `message` | 2 | both | one JSON-RPC message |
| `close` | 3 | both | why, as text, or nothing |
| `ping` | 4 | guest → Mac | |
| `pong` | 5 | Mac → guest | |

Inside the VM, `vibe-guest` serves `/run/vibe/services.sock` (`0666`, as `vibe`
must reach it): a shim's one JSON line, `{"open":"linear","agent":…}`, and its
answer, `{"ok":true}` or `{"ok":false,"error":…}`, then JSON-RPC, one message per
line. It takes no relay, so a process in the guest can't pose as one. With no
relay up it answers that the Mac can't be reached. And it's the shim too: it copies
itself to `/run/vibe/bin/vibe`, and as `vibe mcp-proxy SERVICE` is an MCP server on
stdio. No token or verifier ever reaches the guest.
