#!/usr/bin/env bash
# Wipes the local BIP300/301 regtest stack back to state 0: empty bitcoind regtest chain,
# no enforcer wallet/validator state, no electrs index, no beth chain data. Run
# scripts/regtest-up.sh again afterwards to start fresh.
#
# Refuses to run while any of the four processes are still up (their on-disk state can be
# mid-write). Pass --force to have it stop them for you first, or --yes to skip the
# confirmation prompt (e.g. for scripting).
#
# By default $DATADIR/bitcoin/bitcoin.conf is kept, so custom ports/zmq addresses survive
# a reset -- only the chain itself (blocks/chainstate/wallets, under regtest/) is wiped.
# Pass --purge-config to delete it too (regtest-up.sh will regenerate the default on next run).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DATADIR="${DATADIR:-$ROOT/DATADIR}"

BITCOIN_DATADIR="$DATADIR/bitcoin"
ENFORCER_DATADIR="$DATADIR/enforcer"
ELECTRS_DATADIR="$DATADIR/electrs"
BETH_DATADIR="$DATADIR/beth"
LOGDIR="$DATADIR/logs"

FORCE=0
ASSUME_YES=0
PURGE_CONFIG=0
for arg in "$@"; do
    case "$arg" in
        --force) FORCE=1 ;;
        --yes|-y) ASSUME_YES=1 ;;
        --purge-config) PURGE_CONFIG=1 ;;
        -h|--help)
            sed -n '2,15p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "unknown argument: $arg (expected --force, --yes, --purge-config)" >&2; exit 1 ;;
    esac
done

# ---------------------------------------------------------------------------
# Refuse (or stop) if any of the four are still running -- match by binary path under
# $ROOT so this can't touch an unrelated bitcoind/electrs running elsewhere on the machine.
# ---------------------------------------------------------------------------

RUNNING_PIDS=()
while IFS= read -r pid; do
    [ -n "$pid" ] && RUNNING_PIDS+=("$pid")
done < <(pgrep -f "$ROOT/(bitcoin-patched/build/bin/bitcoind|electrs/target/(release|debug)/electrs|bip300301_enforcer/target/(release|debug)/bip300301_enforcer|beth/target/(release|debug)/beth) " 2>/dev/null || true)

if [ "${#RUNNING_PIDS[@]}" -gt 0 ]; then
    if [ "$FORCE" -ne 1 ]; then
        echo "error: part of the stack is still running (pid(s): ${RUNNING_PIDS[*]})." >&2
        echo "Stop it first (Ctrl-C on regtest-up.sh), or re-run this with --force to stop it for you." >&2
        exit 1
    fi
    echo "Stopping running processes (${RUNNING_PIDS[*]})..."
    kill -TERM "${RUNNING_PIDS[@]}" 2>/dev/null || true
    waited=0
    while kill -0 "${RUNNING_PIDS[@]}" 2>/dev/null; do
        sleep 1
        waited=$((waited + 1))
        if [ "$waited" -ge 20 ]; then
            echo "Still alive after 20s, sending SIGKILL..."
            kill -KILL "${RUNNING_PIDS[@]}" 2>/dev/null || true
            break
        fi
    done
fi

# ---------------------------------------------------------------------------
# Confirm
# ---------------------------------------------------------------------------

echo "This will permanently delete:"
[ -d "$BITCOIN_DATADIR/regtest" ] && echo "  $BITCOIN_DATADIR/regtest  (blocks, chainstate, wallets)"
[ -d "$ENFORCER_DATADIR" ] && echo "  $ENFORCER_DATADIR  (enforcer wallet + validator state)"
[ -d "$ELECTRS_DATADIR" ] && echo "  $ELECTRS_DATADIR  (electrs index)"
[ -d "$BETH_DATADIR" ] && echo "  $BETH_DATADIR  (beth chain db)"
[ -d "$LOGDIR" ] && echo "  $LOGDIR/*  (logs)"
if [ "$PURGE_CONFIG" -eq 1 ]; then
    echo "  $BITCOIN_DATADIR/bitcoin.conf  (--purge-config)"
else
    echo "  (keeping $BITCOIN_DATADIR/bitcoin.conf -- pass --purge-config to remove it too)"
fi

if [ "$ASSUME_YES" -ne 1 ]; then
    read -r -p "Proceed? [y/N] " reply
    case "$reply" in
        y|Y|yes|YES) ;;
        *) echo "Aborted."; exit 1 ;;
    esac
fi

# ---------------------------------------------------------------------------
# Wipe
# ---------------------------------------------------------------------------

rm -rf "$BITCOIN_DATADIR/regtest"
rm -rf "$ENFORCER_DATADIR"
rm -rf "$ELECTRS_DATADIR"
rm -rf "$BETH_DATADIR"
rm -rf "$LOGDIR"
if [ "$PURGE_CONFIG" -eq 1 ]; then
    rm -f "$BITCOIN_DATADIR/bitcoin.conf"
fi

echo "Reset complete. Run scripts/regtest-up.sh to start fresh."
