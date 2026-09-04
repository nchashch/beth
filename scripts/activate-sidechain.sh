#!/usr/bin/env bash
# Activates a sidechain slot against a running bip300301_enforcer + bitcoind regtest stack
# (see scripts/regtest-up.sh): submits a BIP300 sidechain proposal (M1), then mines enough
# blocks with the ACK policy on to pass the activation threshold (M2).
#
# Usage: scripts/activate-sidechain.sh [SIDECHAIN_ID] [TITLE] [DESCRIPTION]
#   SIDECHAIN_ID defaults to 10 (beth's slot in this repo's setup).
#
# Idempotent: does nothing if the slot is already active, and re-uses an already-submitted
# but not-yet-mined proposal instead of submitting a duplicate.
#
# This talks to the enforcer's Connect RPC (JSON-over-HTTP, no code generation needed) and
# to bitcoind via bitcoin-cli (cookie auth, same datadir as regtest-up.sh). It mirrors the
# exact propose/wait/ack/mine sequence bip300301_enforcer's own integration tests use
# (integration_tests/integration_test.rs: propose_sidechain / activate_sidechain).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DATADIR="${DATADIR:-$ROOT/DATADIR}"

SIDECHAIN_ID="${1:-10}"
TITLE="${2:-beth}"
DESCRIPTION="${3:-beth: reth-based BIP300/301 sidechain}"

ENFORCER_GRPC_ADDR="${ENFORCER_GRPC_ADDR:-127.0.0.1:50051}"
BITCOIN_DATADIR="${BITCOIN_DATADIR:-$DATADIR/bitcoin}"
BITCOIN_CLI="${BITCOIN_CLI:-$ROOT/bitcoin-patched/build/bin/bitcoin-cli}"
MINING_WALLET="${MINING_WALLET:-regtest}"

for tool in curl jq; do
    command -v "$tool" >/dev/null || { echo "error: $tool is required" >&2; exit 1; }
done
[ -x "$BITCOIN_CLI" ] || { echo "error: bitcoin-cli not found at $BITCOIN_CLI" >&2; exit 1; }

bitcoin_cli() { "$BITCOIN_CLI" -datadir="$BITCOIN_DATADIR" -regtest "$@"; }

# Call a Connect RPC method with a JSON body; print the JSON response body, fail loudly
# (with that body) on a non-2xx.
rpc() {
    local method="$1" body="$2" resp status
    resp="$(curl -sS -w '\n%{http_code}' -X POST -H 'Content-Type: application/json' \
        --data "$body" "http://$ENFORCER_GRPC_ADDR/cusf.mainchain.v1.$method")"
    status="${resp##*$'\n'}"
    resp="${resp%$'\n'*}"
    if [ "$status" -lt 200 ] || [ "$status" -ge 300 ]; then
        echo "error: $method returned HTTP $status: $resp" >&2
        return 1
    fi
    printf '%s' "$resp"
}

sidechain_active() {
    rpc ValidatorService/GetSidechains '{}' \
        | jq --argjson id "$SIDECHAIN_ID" '[.sidechains[]? | select(.sidechainNumber == $id)] | length > 0'
}

proposal_pending() {
    rpc BlockProducerService/GetBlockProducerState '{}' \
        | jq --argjson id "$SIDECHAIN_ID" '[.pendingProposals[]? | select(.sidechainNumber == $id)] | length > 0'
}

echo "Checking enforcer at $ENFORCER_GRPC_ADDR..."
chain_info="$(rpc ValidatorService/GetChainInfo '{}')"
network="$(jq -r '.network' <<<"$chain_info")"
[ "$network" = "NETWORK_REGTEST" ] || { echo "error: enforcer network is $network, expected NETWORK_REGTEST" >&2; exit 1; }
threshold="$(jq -r '.bip300Constants.unusedSidechainSlotActivationThreshold' <<<"$chain_info")"
blocks_to_activate=$((threshold + 1))

if [ "$(sidechain_active)" = "true" ]; then
    echo "Sidechain $SIDECHAIN_ID is already active. Nothing to do."
    exit 0
fi

if [ "$(proposal_pending)" = "true" ]; then
    echo "Sidechain $SIDECHAIN_ID already has a pending proposal; reusing it."
else
    echo "Submitting sidechain proposal (id=$SIDECHAIN_ID, title=\"$TITLE\")..."
    # hash_id_1/hash_id_2 identify the sidechain's genesis/commitment scheme to a real
    # sidechain implementation; the enforcer's own test suite uses all-zero placeholders
    # for activation (see propose_sidechain in integration_tests/integration_test.rs), which
    # is all activation itself requires.
    hash_id_1="$(printf '0%.0s' $(seq 1 64))" # 32 zero bytes
    hash_id_2="$(printf '0%.0s' $(seq 1 40))" # 20 zero bytes
    proposal_req="$(jq -n \
        --argjson id "$SIDECHAIN_ID" \
        --arg title "$TITLE" \
        --arg desc "$DESCRIPTION" \
        --arg h1 "$hash_id_1" \
        --arg h2 "$hash_id_2" \
        '{sidechainId: $id, declaration: {v0: {title: $title, description: $desc, hashId1: {hex: $h1}, hashId2: {hex: $h2}}}}')"
    rpc BlockProducerService/SubmitSidechainProposal "$proposal_req" >/dev/null

    echo "Waiting for the proposal to be persisted..."
    waited=0
    until [ "$(proposal_pending)" = "true" ]; do
        sleep 1
        waited=$((waited + 1))
        if [ "$waited" -ge 30 ]; then
            echo "error: proposal for sidechain $SIDECHAIN_ID never appeared as pending" >&2
            exit 1
        fi
    done
fi

echo "Setting ACK-all policy so mined blocks carry the M1/M2 commitments..."
rpc BlockProducerService/SetAckAllProposals '{"ackAll": true}' >/dev/null

bitcoin_cli createwallet "$MINING_WALLET" >/dev/null 2>&1 || bitcoin_cli loadwallet "$MINING_WALLET" >/dev/null 2>&1 || true
mining_addr="$(bitcoin_cli -rpcwallet="$MINING_WALLET" getnewaddress)"

echo "Mining 1 block to commit the proposal (M1)..."
rpc MiningService/GenerateToAddress "$(jq -n --arg addr "$mining_addr" '{blocks: 1, address: $addr}')" >/dev/null

echo "Mining $blocks_to_activate block(s) to pass the activation threshold ($threshold) (M2 acks)..."
rpc MiningService/GenerateToAddress \
    "$(jq -n --argjson n "$blocks_to_activate" --arg addr "$mining_addr" '{blocks: $n, address: $addr}')" >/dev/null

if [ "$(sidechain_active)" = "true" ]; then
    echo "Sidechain $SIDECHAIN_ID is now active."
else
    echo "warning: sidechain $SIDECHAIN_ID did not activate. Block producer state:" >&2
    rpc BlockProducerService/GetBlockProducerState '{}' | jq . >&2
    exit 1
fi
