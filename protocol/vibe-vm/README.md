# The `vibe-vm` contract

`vibe-vm` is the Swift helper that runs one `linux` agent's VM on the Mac,
on Virtualization.framework (PRD: Architecture; Sleep and wake). `vibed`
drives it through `vibe_env::vm`. This directory pins what passes between
them, and both sides are tested against it:

| File | What it pins |
| --- | --- |
| `config.json` | An agent's `vm.json`: the VM's configuration, fixed for its life |
| `examples/*.json` | The lines `vibe-vm` prints when it starts (`command` and `ready`), and requests on its control socket with their exact replies (`request` or a malformed `request_raw`, and `reply`) |

The checks: `crates/vibe-env/tests/contract.rs` (Rust) and
`swift/vibe-vm/Tests/VibeVMKitTests/ContractTests.swift` (Swift) decode
every file into their types and encode it back to the same JSON, check
exactly the optional config fields can be left out, and require an example
of every error code, request and start. `crates/vibe-env/tests/vm_cycle.rs`
then drives the real thing on a Mac (`scripts/check-full.sh`, step
`vm-cycle`).

## The configuration

`vm.json` lives in the agent's directory, `~/VibeOS/agents/<ref>/vm.json`.
`vibed` writes it once, when it creates the agent: vCPUs from the host
limits (`cpu_cores`, at most the Mac's), RAM from the profile, the kernel
and initrd, the disks (the read-only layer images, then the agent's own
writable image) and the virtio-fs shares (the worktree, the project's
`.git`, the agent's Claude home, the package caches). A disk's `id` is its
virtio serial, which the guest finds it by; a share's `tag` is what the
guest mounts.

A snapshot only restores into the exact configuration it was saved from,
and the random defaults Virtualization.framework would pick for the machine
identifier and the MAC address differ on every start; the random MAC is
what broke restores in the spike. So `vibe-vm create` fills both in, once,
and writes them back into `vm.json`; `boot` and `restore` refuse a
configuration without them. Those two never change. The rest of `vm.json`
is fixed from a save until its restore; before a fresh boot `vibed` may
change the disks (newer layer versions), `cpus` or `memory`.

## Commands

```
vibe-vm create  --config VM.JSON
vibe-vm boot    --config VM.JSON --socket run/<ref>.vm
vibe-vm restore --config VM.JSON --socket run/<ref>.vm
vibe-vm check
```

**`create`** gives the VM its machine identifier and, if it has none, a
random locally administered MAC address; checks Virtualization.framework
accepts the VM, with `validate()` and `validateSaveRestoreSupport()`; saves
`vm.json`; prints one line, `{"ok":true,"machine_id":…,"mac":…}` or a
failure, and exits 0 or 1.

**`boot`** and **`restore`** start the VM and keep running: `vibe-vm` calls
`setsid()` first, so it's detached from `vibed` like a shim, and `vibed`
collects its exit. `vibed` starts it with stdout on a pipe and stderr on
its log (`vibe-vm.log` in the agent's directory). Once the VM runs,
`vibe-vm` prints one ready line on the pipe, with its pid and timings, and
writes nothing more there; or it prints a failure and exits 1. It serves
its control socket from before the VM starts until it stops, so a probe
during a long restore finds it restoring. A restarted `vibed` finds it
again by that socket.

`restore` reads the snapshot (`snapshot` in `vm.json`), deletes it, and
only then resumes the VM: the VM's disks move on from that moment, so the
snapshot must never be restored again. For the same reason `boot` deletes
any snapshot the VM still has before it starts. A snapshot that won't restore is
kept, with the error code `restore_failed`, for `vibed` to discard and
resume the agent with `claude --resume` in a fresh VM (PRD: Failures).

## The control socket

`run/<ref>.vm`, next to the shims' `run/<ref>.sock`: no longer, so it fits
the 104-byte limit wherever a session's socket does, and never taken for a
shim's by a session scan. It's `0600` in the private run directory, and
`vibe-vm` holds `run/<ref>.vm.lock` for its whole life, so there's one per
VM, and a probe can tell a dead one by the free lock (as for shims). It
removes the socket and lets the lock go just before it exits, so a restore
can follow a sleep at once.

A connection carries one request line, sent within 30 s, and gets one reply
line, both JSON.
Every reply is `{"ok":true, …}` or `{"ok":false,"code":…,"error":…}`.

| Request | Does | Reply |
| --- | --- | --- |
| `{"op":"status"}` | | `ref`, `pid`, `state`, `started` (`boot` or `restore`), `protocol` |
| `{"op":"connect","port":N}` | Makes the connection a byte stream to vsock port N in the guest, both ways, with half-closes passed on: a guest that sees its input end should finish and close. This is how a shim reaches `vibe-guest`, since only the process that owns a VM can reach its vsock. A port nobody answers on fails within 5 s | `{"ok":true}`, then the stream |
| `{"op":"sleep","balloon_ms":N,"balloon_floor":B}` | `vibe-guest` must have frozen the agent first. Inflates the memory balloon down to B bytes (absent: the least the VM may have; `vibed` should pass what the guest really uses plus headroom, so nothing in it runs short), waits N ms (2000 if absent; 0 skips the balloon), lets it go, pauses, saves the VM to `.<ref>.partial` beside the snapshot (no ref starts with a dot, so it's never another agent's snapshot), flushes it to disk, makes it `0600` (it holds all the guest's memory, secrets included), renames it into place, stops the VM and exits | The snapshot's path and size on disk, and how long each step took, sent once the VM is stopped. If the save fails, nothing is left behind, the VM resumes and the code is `save_failed` |
| `{"op":"stop"}` | Stops the VM at once, without saving, and exits | `{"ok":true}` |

While the VM starts, restores, sleeps or stops, and after a sleep until
`vibe-vm` exits, requests that would change it are refused with `busy`; a request the VM's state doesn't allow (a
`connect` to a paused VM, say) with `bad_state`. If the guest stops the VM
itself, `vibe-vm` removes its socket and exits: 0 for a power-off, 1 for an
error.

**Why the balloon.** A snapshot holds every page the guest has touched,
including those it has since freed: 1.09 GB in the spike after the guest
used and freed 1 GB. Inflating the balloon takes the free pages from the
guest and releases them on the host; letting it go before the pause hands
them back untouched, so they stay out of the snapshot (41 MB in the spike).
Virtualization.framework doesn't say how far the balloon got, so `vibe-vm`
waits a fixed time.
