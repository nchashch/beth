#!/usr/bin/env bash
# Clones and builds this devnet's sibling repos (bitcoin-patched, electrs), fetches and builds
# this repo's bip300301_enforcer git submodule, and creates an empty $DATADIR -- everything
# scripts/regtest-up.sh needs before it can start the stack (see scripts/README.md for the full
# picture). Doesn't build beth itself (you're already in it -- `cargo build --release`, which
# needs the submodule this script fetches too) or install Foundry (see scripts/install-foundry.sh
# for that).
#
# Safe to re-run: skips cloning a repo (or initializing the submodule) that's already checked
# out (an existing checkout, and any local changes in it, is left alone -- this never
# fetches/pulls/resets one), and every build below is incremental via cargo's/cmake's own
# caching either way.
#
# Usage: scripts/setup-workspace.sh
#   Override BITCOIN_PATCHED_REPO / ELECTRS_REPO to clone a fork instead of the defaults below
#   (e.g. the git@github.com:... SSH form instead of https://, if you have push access and a key
#   set up). Override JOBS to control build parallelism (defaults to nproc).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BETH_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
ROOT="$(cd "$BETH_DIR/.." && pwd)"

BITCOIN_PATCHED_REPO="${BITCOIN_PATCHED_REPO:-https://github.com/LayerTwo-Labs/bitcoin-patched.git}"
ELECTRS_REPO="${ELECTRS_REPO:-https://github.com/mempool/electrs}"

JOBS="${JOBS:-$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)}"

for tool in git cmake cargo; do
    command -v "$tool" >/dev/null || { echo "error: $tool is required" >&2; exit 1; }
done

clone_if_missing() {
    local dir="$1" repo="$2"
    if [ -d "$dir/.git" ]; then
        echo "  $dir already cloned, leaving it alone."
    else
        echo "  Cloning $repo -> $dir"
        git clone "$repo" "$dir"
    fi
}

echo "== Cloning sibling repos into $ROOT =="
clone_if_missing "$ROOT/bitcoin-patched" "$BITCOIN_PATCHED_REPO"
clone_if_missing "$ROOT/electrs" "$ELECTRS_REPO"

echo
echo "== Fetching the bip300301_enforcer submodule (pinned in $BETH_DIR/.gitmodules) =="
# bip300301_enforcer is vendored as a git submodule of beth itself (see build.rs and
# .gitmodules), the same way thunder-rust vendors it -- pinned to a specific, reviewed commit,
# not "whatever's cloned as a sibling". Only initialize it if it isn't already (mirrors
# clone_if_missing's restraint above): a plain `git submodule update` on an already-initialized
# submodule would reset it to the pinned commit, discarding any local checkout there on purpose.
ENFORCER_DIR="$BETH_DIR/bip300301_enforcer"
if [ -d "$ENFORCER_DIR/.git" ] || [ -f "$ENFORCER_DIR/.git" ]; then
    echo "  $ENFORCER_DIR already initialized, leaving it alone."
else
    (cd "$BETH_DIR" && git submodule update --init -- bip300301_enforcer)
fi

echo
echo "== Building bitcoin-patched (CMake, -DWITH_ZMQ=ON) =="
# WITH_ZMQ defaults OFF upstream, but bip300301_enforcer depends on bitcoind's ZMQ
# notifications (see scripts/regtest-up.sh's generated bitcoin.conf) -- not optional here.
# Needs libzmq3-dev (or your distro's equivalent) installed to succeed; see scripts/README.md.
cmake -B "$ROOT/bitcoin-patched/build" -S "$ROOT/bitcoin-patched" -DWITH_ZMQ=ON
cmake --build "$ROOT/bitcoin-patched/build" -j"$JOBS"

echo
echo "== Building bip300301_enforcer (cargo build --release, from the pinned submodule) =="
# Built from beth's own submodule, not a separately-cloned copy, so the running enforcer always
# matches the proto definitions beth's own build compiled against.
(cd "$ENFORCER_DIR" && cargo build --release)

echo
echo "== Building electrs (cargo build --release --bin electrs) =="
# --bin electrs: the crate builds more than one binary; this is the one regtest-up.sh runs.
(cd "$ROOT/electrs" && cargo build --release --bin electrs)

echo
echo "== Creating $ROOT/DATADIR =="
mkdir -p "$ROOT/DATADIR"

BITCOIND="$ROOT/bitcoin-patched/build/bin/bitcoind"
ENFORCER_BIN="$ENFORCER_DIR/target/release/bip300301_enforcer"
ELECTRS_BIN="$ROOT/electrs/target/release/electrs"
for bin in "$BITCOIND" "$ENFORCER_BIN" "$ELECTRS_BIN"; do
    [ -x "$bin" ] || { echo "error: expected $bin after building but it's not there" >&2; exit 1; }
done

echo
echo "================================================================"
echo " Workspace ready:"
echo "   bitcoind            = $BITCOIND"
echo "   bitcoin-cli         = $ROOT/bitcoin-patched/build/bin/bitcoin-cli"
echo "   bip300301_enforcer  = $ENFORCER_BIN"
echo "   electrs             = $ELECTRS_BIN"
echo "   DATADIR             = $ROOT/DATADIR"
echo
echo " Still needed before scripts/regtest-up.sh:"
echo "   - beth itself, if you haven't already: (cd beth && cargo build --release)"
echo "   - Foundry (cast/forge), if you don't have it: scripts/install-foundry.sh"
echo "================================================================"
