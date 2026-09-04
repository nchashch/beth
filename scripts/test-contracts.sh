#!/usr/bin/env bash
# Tests beth's EVM against Foundry's own standard project template -- the exact scaffold
# `forge init` generates (src/Counter.sol, script/Counter.s.sol, test/Counter.t.sol, forge-std) --
# rather than anything custom-written for this repo. The point is to exercise beth with an
# unmodified, off-the-shelf toolchain workflow, not this repo's own contracts.
#
# Requires a funded `cast` wallet to pay for the real deployment/interaction transactions below,
# so this first deposits to it via the usual mechanism (scripts/test-deposit.sh, which itself
# defaults its recipient to that same wallet -- see scripts/regtest-up.sh).
#
# Needs the regtest stack up (scripts/regtest-up.sh), the target sidechain active
# (scripts/activate-sidechain.sh), and scripts/bmm-mine.sh already running against it -- nothing
# here drives beth's block production, and both the deposit crediting and every transaction below
# need real beth blocks to confirm.
#
# Two things get tested, deliberately by different mechanisms:
#   1. `forge script ... --broadcast`: deploys Counter.sol and sends a real `increment()` tx,
#      signed by the cast wallet, through beth's actual execution/consensus pipeline (real
#      transactions, real BMM-mined blocks) -- proves beth can serve as a genuine deployment
#      target for a standard Foundry broadcast workflow.
#   2. `forge test --fork-url`: runs the template's own test/Counter.t.sol suite
#      (test_Increment, testFuzz_SetNumber) against an in-memory fork of beth's *live* state --
#      this sends no real transactions (forge's local revm executes everything), so it instead
#      proves beth's JSON-RPC read surface (eth_getBlockByNumber, eth_call, eth_getCode,
#      eth_getProof, etc.) is faithful enough for an unmodified third-party tool to fork from.
#
# Usage: scripts/test-contracts.sh [SIDECHAIN_ID]   (defaults to 10, beth's slot in this repo's
#                                                     setup)
#
# Safe to re-run: the scaffolded project at contracts-test/ is only created once (forge init
# refuses a non-empty directory), and every deploy below is a fresh contract instance.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
DATADIR="${DATADIR:-$ROOT/DATADIR}"
PROJECT_DIR="${PROJECT_DIR:-$ROOT/contracts-test}"

SIDECHAIN_ID="${1:-10}"

ENFORCER_GRPC_ADDR="${ENFORCER_GRPC_ADDR:-127.0.0.1:50051}"
BETH_HTTP_ADDR="${BETH_HTTP_ADDR:-127.0.0.1:8545}"
BETH_RPC_URL="http://$BETH_HTTP_ADDR"
WALLET_DIR="${WALLET_DIR:-$DATADIR/wallet}"
WALLET_NAME="${WALLET_NAME:-regtest}"
FUND_TIMEOUT=120 # seconds to wait for the deposit above to actually get credited on beth

for tool in curl jq; do
    command -v "$tool" >/dev/null || { echo "error: $tool is required" >&2; exit 1; }
done

# Same lookup as scripts/regtest-up.sh and scripts/test-deposit.sh, extended to also find forge.
find_foundry_bin() {
    if command -v cast >/dev/null 2>&1; then dirname "$(command -v cast)"; return 0; fi
    local candidates=()
    [ -n "${XDG_CONFIG_HOME:-}" ] && candidates+=("$XDG_CONFIG_HOME/.foundry/bin")
    candidates+=("$HOME/.config/.foundry/bin" "$HOME/.foundry/bin")
    local dir
    for dir in "${candidates[@]}"; do
        [ -x "$dir/cast" ] && { printf '%s' "$dir"; return 0; }
    done
    return 1
}
FOUNDRY_BIN="$(find_foundry_bin || true)"
if [ -z "$FOUNDRY_BIN" ] || [ ! -x "$FOUNDRY_BIN/forge" ]; then
    echo "error: forge/cast not found. Run scripts/install-foundry.sh first." >&2
    exit 1
fi
CAST="$FOUNDRY_BIN/cast"
FORGE="$FOUNDRY_BIN/forge"

[ -f "$WALLET_DIR/$WALLET_NAME" ] || {
    echo "error: no cast wallet found at $WALLET_DIR/$WALLET_NAME. Run scripts/regtest-up.sh first." >&2
    exit 1
}
WALLET_ADDR="$("$CAST" wallet address --keystore "$WALLET_DIR/$WALLET_NAME" --password-file "$WALLET_DIR/password.txt")"

echo "Checking enforcer at $ENFORCER_GRPC_ADDR..."
active="$(curl -sS -X POST -H 'Content-Type: application/json' --data '{}' \
    "http://$ENFORCER_GRPC_ADDR/cusf.mainchain.v1.ValidatorService/GetSidechains" \
    | jq --argjson id "$SIDECHAIN_ID" '[.sidechains[]? | select(.sidechainNumber == $id)] | length > 0')"
if [ "$active" != "true" ]; then
    echo "error: sidechain $SIDECHAIN_ID is not active. Run scripts/activate-sidechain.sh $SIDECHAIN_ID first." >&2
    exit 1
fi

echo "Checking beth at $BETH_HTTP_ADDR..."
curl -sS -X POST -H 'Content-Type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' \
    "$BETH_RPC_URL" | jq -e '.result' >/dev/null || { echo "error: could not reach beth at $BETH_HTTP_ADDR" >&2; exit 1; }

get_balance() {
    curl -sS -X POST -H 'Content-Type: application/json' \
        --data "$(jq -n --arg a "$WALLET_ADDR" '{jsonrpc:"2.0",id:1,method:"eth_getBalance",params:[$a,"latest"]}')" \
        "$BETH_RPC_URL" | jq -r '.result // "0x0"'
}

echo
echo "Depositing to $WALLET_ADDR via the usual mechanism (scripts/test-deposit.sh)..."
"$SCRIPT_DIR/test-deposit.sh" "$SIDECHAIN_ID"

if [ "$(get_balance)" = "0x0" ]; then
    echo
    echo "Waiting for the deposit to be credited on beth (needs scripts/bmm-mine.sh $SIDECHAIN_ID"
    echo "  running elsewhere to actually build the crediting block)..."
    waited=0
    while [ "$(get_balance)" = "0x0" ]; do
        sleep 2
        waited=$((waited + 2))
        if [ "$waited" -ge "$FUND_TIMEOUT" ]; then
            echo "error: $WALLET_ADDR still has 0 balance after ${FUND_TIMEOUT}s. Is scripts/bmm-mine.sh" >&2
            echo "  $SIDECHAIN_ID running against this same stack?" >&2
            exit 1
        fi
    done
fi
echo "Wallet funded: $(("$(get_balance)")) wei."

echo
if [ -d "$PROJECT_DIR/src" ]; then
    echo "Reusing existing scaffold at $PROJECT_DIR"
else
    echo "Scaffolding Foundry's standard project template at $PROJECT_DIR (forge init)..."
    "$FORGE" init "$PROJECT_DIR"
fi

echo
echo "== forge script: deploying Counter.sol + increment() via a real broadcast tx =="
echo "  (blocks on each tx's receipt -- make sure scripts/bmm-mine.sh $SIDECHAIN_ID is running)"
(
    cd "$PROJECT_DIR"
    "$FORGE" script script/Counter.s.sol:CounterScript \
        --rpc-url "$BETH_RPC_URL" \
        --keystore "$WALLET_DIR/$WALLET_NAME" --password-file "$WALLET_DIR/password.txt" \
        --broadcast --slow -vv
)

chain_id="$("$CAST" chain-id --rpc-url "$BETH_RPC_URL")"
broadcast_file="$PROJECT_DIR/broadcast/Counter.s.sol/$chain_id/run-latest.json"
[ -f "$broadcast_file" ] || { echo "error: no broadcast artifact at $broadcast_file" >&2; exit 1; }
counter_addr="$(jq -r '.transactions[] | select(.contractName == "Counter") | .contractAddress' "$broadcast_file" | tail -1)"
[ -n "$counter_addr" ] && [ "$counter_addr" != "null" ] || { echo "error: could not find Counter's deployed address in $broadcast_file" >&2; exit 1; }
echo "Counter deployed at $counter_addr"

initial="$("$CAST" call --rpc-url "$BETH_RPC_URL" "$counter_addr" 'number()(uint256)')"
echo "  number() after deploy: $initial (expected 0)"
[ "$initial" = "0" ] || { echo "error: expected 0, got $initial" >&2; exit 1; }

echo "Sending a real increment() tx (blocks until beth mines it)..."
"$CAST" send --keystore "$WALLET_DIR/$WALLET_NAME" --password-file "$WALLET_DIR/password.txt" \
    --rpc-url "$BETH_RPC_URL" "$counter_addr" 'increment()' >/dev/null

after="$("$CAST" call --rpc-url "$BETH_RPC_URL" "$counter_addr" 'number()(uint256)')"
echo "  number() after increment(): $after (expected 1)"
if [ "$after" != "1" ]; then
    echo "error: expected 1, got $after" >&2
    exit 1
fi
echo "Confirmed: a real broadcast deployment and a real follow-up tx both executed correctly on beth."

echo
echo "== forge test --fork-url: running the template's own test suite against a live fork of beth =="
(
    cd "$PROJECT_DIR"
    "$FORGE" test --fork-url "$BETH_RPC_URL" -vv
)
echo
echo "All checks passed: beth deploys, executes, and is fork-compatible with an unmodified Foundry toolchain."
