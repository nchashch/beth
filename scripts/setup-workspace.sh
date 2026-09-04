#!/usr/bin/env bash
# Clones and builds this devnet's sibling repos (bitcoin-patched, bip300301_enforcer, electrs)
# into the workspace root alongside this repo, and creates an empty $DATADIR -- everything
# scripts/regtest-up.sh needs before it can start the stack (see scripts/README.md for the full
# picture). Doesn't touch beth itself (you're already in it -- build it the usual way,
# `cargo build --release`) or install Foundry (see scripts/install-foundry.sh for that).
#
# Safe to re-run: skips cloning a repo that's already checked out (an existing checkout, and any
# local changes in it, is left alone -- this never fetches/pulls/resets one), and every build
# below is incremental via cargo's/cmake's own caching either way.
#
# Usage: scripts/setup-workspace.sh
#   Override BITCOIN_PATCHED_REPO / ENFORCER_REPO / ELECTRS_REPO to clone a fork instead of the
#   defaults below. Override JOBS to control build parallelism (defaults to nproc).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

BITCOIN_PATCHED_REPO="${BITCOIN_PATCHED_REPO:-git@github.com:LayerTwo-Labs/bitcoin-patched.git}"
ENFORCER_REPO="${ENFORCER_REPO:-git@github.com:LayerTwo-Labs/bip300301_enforcer.git}"
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
clone_if_missing "$ROOT/bip300301_enforcer" "$ENFORCER_REPO"
clone_if_missing "$ROOT/electrs" "$ELECTRS_REPO"

echo
echo "== Building bitcoin-patched (CMake, -DWITH_ZMQ=ON) =="
# WITH_ZMQ defaults OFF upstream, but bip300301_enforcer depends on bitcoind's ZMQ
# notifications (see scripts/regtest-up.sh's generated bitcoin.conf) -- not optional here.
# Needs libzmq3-dev (or your distro's equivalent) installed to succeed; see scripts/README.md.
cmake -B "$ROOT/bitcoin-patched/build" -S "$ROOT/bitcoin-patched" -DWITH_ZMQ=ON
cmake --build "$ROOT/bitcoin-patched/build" -j"$JOBS"

echo
echo "== Building bip300301_enforcer (cargo build --release) =="
(cd "$ROOT/bip300301_enforcer" && cargo build --release)

echo
echo "== Building electrs (cargo build --release --bin electrs) =="
# --bin electrs: the crate builds more than one binary; this is the one regtest-up.sh runs.
(cd "$ROOT/electrs" && cargo build --release --bin electrs)

echo
echo "== Creating $ROOT/DATADIR =="
mkdir -p "$ROOT/DATADIR"

BITCOIND="$ROOT/bitcoin-patched/build/bin/bitcoind"
ENFORCER_BIN="$ROOT/bip300301_enforcer/target/release/bip300301_enforcer"
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
