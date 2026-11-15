//! An agent building a scratch rootfs leaves the server's mounts alone
//! (#169). The agent, running as root, bind-mounts `/sys` and `/dev`
//! recursively into a rootfs in its temp dir and unmounts them again, as a
//! chroot build does. On a server whose mounts are shared, that took the
//! server's `/sys/fs/cgroup`, `/dev/pts` and `/dev/shm` with it.
//!
//! Nothing here touches this machine's own mounts. Everything happens in a
//! sandbox: a new user and mount namespace (`unshare --user --map-root-user
//! --mount --propagation private`), where a tmpfs stands in for the server
//! with `sys/fs/cgroup`, `dev/pts` and `dev/shm` mounted under it and all of
//! it shared, as systemd leaves `/`. A mount namespace owned by a new user
//! namespace can't propagate to the one it was copied from in any case.
//!
//! First the agent's steps run in the sandbox's namespace as they did
//! before, to show they do unmount the stand-in server's mounts; then they
//! run under a real `vibe-shim`, as an agent's command does, and the
//! server's mounts must all still be there.
//!
//! Where user namespaces aren't allowed (Ubuntu 24.04's AppArmor default,
//! for one, unless run as root) the test says so and passes, unless
//! `VIBE_TEST_MOUNTS=1` asks for it to run, as `scripts/check-full.sh` does.

#![cfg(target_os = "linux")]

use std::process::{Command, Output};

const SHIM: &str = env!("CARGO_BIN_EXE_vibe-shim");

/// The server's mounts the test watches, under the stand-in's root.
const WATCHED: [&str; 3] = ["sys/fs/cgroup", "dev/pts", "dev/shm"];

/// Run in the sandbox as `sh sandbox.sh <dir> <outer mount ns> <shim>`. It
/// prints `<phase> <mount> mounted|GONE` for each watched mount after each
/// phase, and `agent <phase> <what it saw>` for what the agent did.
const SANDBOX: &str = r#"set -eu
dir=$1 outer=$2 shim=$3
# Never anywhere but a mount namespace of the sandbox's own.
[ "$(readlink /proc/self/ns/mnt)" != "$outer" ] || { echo "not in a new mount namespace"; exit 99; }
H=$dir/server
R=$H/root/VibeOS/agents/quiet-amber-otter/tmp/claude-0/session/scratchpad/rootfs

server() {
  mount -t tmpfs server "$H"
  for d in sys dev; do mkdir "$H/$d"; mount -t tmpfs "$d" "$H/$d"; done
  for m in sys/fs/cgroup dev/pts dev/shm; do mkdir -p "$H/$m"; mount -t tmpfs "${m##*/}" "$H/$m"; done
  # Shared, as systemd mounts / and everything under it.
  mount --make-rshared "$H"
  mkdir -p "$R/sys" "$R/dev"
}

report() {
  for m in sys/fs/cgroup dev/pts dev/shm; do
    if mountpoint -q "$H/$m"; then echo "$1 $m mounted"; else echo "$1 $m GONE"; fi
  done
}

# What the agent does, then says whether it saw its own copies mounted.
cat >"$dir/agent.sh" <<EOF
set -eu
mount --rbind "$H/sys" "$R/sys"
mount --rbind "$H/dev" "$R/dev"
bound=no
mountpoint -q "$R/sys/fs/cgroup" && mountpoint -q "$R/dev/pts" && mountpoint -q "$R/dev/shm" && bound=yes
umount -R "$R/sys"
umount -R "$R/dev"
echo "\$bound" >"$dir/agent.\$1"
EOF

mkdir -p "$H"
server
sh "$dir/agent.sh" uncontained
echo "agent uncontained $(cat "$dir/agent.uncontained")"
report uncontained
umount -R "$H"
server

"$shim" --ref quiet-amber-otter --socket "$dir/run/quiet-amber-otter.sock" \
  --state "$dir/agents/quiet-amber-otter" --cwd "$dir" \
  -- sh "$dir/agent.sh" contained >/dev/null
i=0
until [ -f "$dir/agent.contained" ]; do
  i=$((i + 1))
  [ $i -lt 400 ] || { echo "the agent under the shim didn't finish"; cat "$dir/agents/quiet-amber-otter/"*.log 2>/dev/null; exit 98; }
  sleep 0.05
done
echo "agent contained $(cat "$dir/agent.contained")"
report contained
"#;

fn unshare(args: &[&str]) -> Command {
    let mut cmd = Command::new("unshare");
    cmd.args([
        "--user",
        "--map-root-user",
        "--mount",
        "--propagation",
        "private",
        "--",
    ])
    .args(args);
    cmd
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Whether the sandbox can be had here; says why not if it can't.
fn sandbox_allowed() -> bool {
    let required = std::env::var("VIBE_TEST_MOUNTS").is_ok_and(|v| v == "1");
    let why = match unshare(&["true"]).output() {
        Ok(out) if out.status.success() => return true,
        Ok(out) => text(&out),
        Err(e) => format!("can't run unshare: {e}"),
    };
    assert!(
        !required,
        "VIBE_TEST_MOUNTS=1, but a user and mount namespace can't be had here: {why}"
    );
    eprintln!("skipped: a user and mount namespace can't be had here ({why})");
    false
}

/// A copy of the shim in `dir`, where the sandbox can run it: its root is
/// only this user, and where the test runs as root (by sudo, in CI) the
/// build's `/home/<user>` belongs to a user it has no mapping for, so it
/// can't so much as look in it.
fn shim_in(dir: &std::path::Path) -> String {
    let copy = dir.join("vibe-shim");
    std::fs::copy(SHIM, &copy).unwrap();
    copy.to_string_lossy().into_owned()
}

fn line_for(output: &str, phase: &str, mount: &str) -> String {
    let prefix = format!("{phase} {mount} ");
    output
        .lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("nothing said of {mount} {phase}:\n{output}"))
        .to_owned()
}

/// Which of this machine's own watched mounts are mounted.
fn own_mounts() -> Vec<(String, bool)> {
    WATCHED
        .iter()
        .map(|m| {
            let path = format!("/{m}");
            let mounted = Command::new("mountpoint")
                .args(["-q", &path])
                .status()
                .is_ok_and(|s| s.success());
            (path, mounted)
        })
        .collect()
}

#[test]
fn a_scratch_rootfs_torn_down_by_an_agent_leaves_the_servers_mounts() {
    if !sandbox_allowed() {
        return;
    }
    // Under /tmp, as the shim's socket path has to be short.
    let dir = tempfile::Builder::new()
        .prefix("mounts")
        .tempdir_in("/tmp")
        .unwrap();
    let script = dir.path().join("sandbox.sh");
    std::fs::write(&script, SANDBOX).unwrap();
    std::fs::create_dir(dir.path().join("server")).unwrap();
    let outer = std::fs::read_link("/proc/self/ns/mnt").unwrap();
    let before = own_mounts();
    let out = unshare(&[
        "sh",
        &script.to_string_lossy(),
        &dir.path().to_string_lossy(),
        &outer.to_string_lossy(),
        &shim_in(dir.path()),
    ])
    .output()
    .unwrap();
    let output = text(&out);
    eprintln!("{output}");
    assert!(out.status.success(), "the sandbox failed:\n{output}");

    // The agent really did bind and unmount the server's mounts, both times.
    assert!(output.contains("agent uncontained yes"), "{output}");
    assert!(output.contains("agent contained yes"), "{output}");
    for mount in WATCHED {
        // As before the fix: the teardown reached the server.
        assert_eq!(
            line_for(&output, "uncontained", mount),
            "GONE",
            "the sandbox didn't reproduce #169, so it proves nothing:\n{output}"
        );
        // Under the shim, it doesn't.
        assert_eq!(
            line_for(&output, "contained", mount),
            "mounted",
            "an agent under the shim unmounted the server's {mount}:\n{output}"
        );
    }
    // And this machine's own were never in reach.
    assert_eq!(own_mounts(), before, "this machine's own mounts changed");
}

/// Run in the sandbox as `sh no-pts.sh <dir> <outer mount ns> <shim>`: a
/// `/dev` with no `/dev/pts` mounted, its `/dev/ptmx` the link into it that
/// many servers have, then the shim.
const NO_PTS: &str = r#"set -eu
dir=$1 outer=$2 shim=$3
[ "$(readlink /proc/self/ns/mnt)" != "$outer" ] || { echo "not in a new mount namespace"; exit 99; }
mount -t tmpfs dev /dev
touch /dev/null
mount --bind "$dir/null" /dev/null
mkdir /dev/pts
ln -s pts/ptmx /dev/ptmx
"$shim" --ref quiet-amber-otter --socket "$dir/run/quiet-amber-otter.sock" \
  --state "$dir/agents/quiet-amber-otter" --cwd "$dir" -- /usr/bin/systemd-run --user true
"#;

/// The shim says the terminal is what's missing, not the command: in #169
/// it said `can't start /usr/bin/systemd-run: No such device`.
#[test]
fn a_server_without_dev_pts_is_told_that_rather_than_blaming_the_command() {
    if !sandbox_allowed() {
        return;
    }
    let dir = tempfile::Builder::new()
        .prefix("nopts")
        .tempdir_in("/tmp")
        .unwrap();
    let script = dir.path().join("no-pts.sh");
    std::fs::write(&script, NO_PTS).unwrap();
    std::fs::write(dir.path().join("null"), "").unwrap();
    let outer = std::fs::read_link("/proc/self/ns/mnt").unwrap();
    let out = unshare(&[
        "sh",
        "-c",
        // /dev's own null, bound before /dev is covered.
        "mount --bind /dev/null \"$1/null\" && exec sh \"$2\" \"$1\" \"$3\" \"$4\"",
        "sh",
        &dir.path().to_string_lossy(),
        &script.to_string_lossy(),
        &outer.to_string_lossy(),
        &shim_in(dir.path()),
    ])
    .output()
    .unwrap();
    let output = text(&out);
    eprintln!("{output}");
    assert!(
        !out.status.success(),
        "the shim started without /dev/pts:\n{output}"
    );
    assert!(
        output.contains("/dev/pts isn't mounted on this server"),
        "{output}"
    );
    assert!(output.contains("mount -t devpts"), "{output}");
    assert!(!output.contains("can't start"), "{output}");
}
