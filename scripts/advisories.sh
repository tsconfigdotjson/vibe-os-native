#!/usr/bin/env bash
# Cargo.lock against RustSec, today's copy of it, by the rules in deny.toml
# (PRD: Auth, security and trust, Dependencies). check-full.sh runs this, so CI
# does on every merge, and the release workflow before it publishes anything.
#
# Nothing to install by hand, and no package manager: cargo-deny at this exact
# version with its lockfile, built once by cargo from crates.io into the cache
# zig-tools.sh uses, and always this one, so an answer doesn't depend on the machine.
# It can't answer without the network, and that's a failure, not a pass.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

CARGO_DENY_VERSION=0.20.2

root=${VIBE_TOOLS_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/vibeos/tools}/cargo-deny-$CARGO_DENY_VERSION
if ! [ -x "$root/bin/cargo-deny" ]; then
  echo "advisories: building cargo-deny $CARGO_DENY_VERSION, once" >&2
  cargo install cargo-deny --locked --version "$CARGO_DENY_VERSION" --root "$root" --quiet
fi
# Whether a crate was yanked comes from cargo's index cache, which nothing else
# here refreshes, and CI restores an old one. A dry run writes no lockfile.
cargo update --dry-run --quiet
# Without its config, even one named with --config, cargo-deny falls back to
# weaker rules and can pass.
[ -f deny.toml ] || { echo "advisories: no deny.toml" >&2; exit 1; }
# --locked, as what ships is what's in Cargo.lock, not what cargo would pick today.
exec "$root/bin/cargo-deny" --color never --locked check advisories
