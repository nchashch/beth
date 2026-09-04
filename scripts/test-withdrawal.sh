#!/usr/bin/env bash
# Tests a real BIP300 withdrawal (beth sidechain -> Bitcoin mainchain) end-to-end against a
# running bip300301_enforcer + bitcoind + beth regtest stack (see scripts/regtest-up.sh), for a
# sidechain slot that already has treasury funds (run scripts/activate-sidechain.sh then
# scripts/test-deposit.sh first if you haven't).
#
# Unlike an earlier version of this script, this one does NOT hand-build a withdrawal bundle and
# broadcast it directly to the enforcer -- that bypassed beth's own withdrawal machinery entirely
# (there was no real L2 WithdrawalRequestQueue request behind it), which was the only option back
# when nothing in this repo drove beth's own block production. Now that scripts/bmm-mine.sh
# exists, this tests the real path:
#   1. A real EIP-1559 tx, signed by the `cast` wallet scripts/regtest-up.sh sets up, calls
#      WithdrawalRequestQueue.requestWithdrawal(bytes,uint256) with real ETH value.
#   2. beth's own payload builder (see beth/src/payload.rs) notices the pending request on its
#      very next block build and broadcasts the resulting M6 bundle to the enforcer itself --
#      nothing here does that step manually.
#   3. Mainchain blocks mined *through the enforcer* (bmm-mine.sh does this once a second anyway,
#      as part of its own BMM-bid-confirming loop) carry M4 votes automatically under the
#      enforcer's ACK-all policy, same as scripts/bmm-mine.sh's own BMM bids -- a block mined
#      directly via plain bitcoin-cli would carry none of the enforcer's BIP300 coinbase messages
#      and would never advance the bundle's vote count.
#   4. Once the bundle's vote count passes the network's inclusion threshold, the enforcer pays it
#      out on L1, and on beth's next block, `systemReportBundleLifecycle` (a SYSTEM_ADDRESS-gated
#      call beth itself issues) marks the request STATUS_CONFIRMED on-chain.
#
# This script only submits step 1 and polls for the STATUS_CONFIRMED outcome of steps 2-4 -- it
# needs scripts/bmm-mine.sh (or equivalent) *already running* against the same sidechain, since
# that's what actually drives both beth's block production and the mainchain vote-carrying blocks.
# Without it, the request will just sit at STATUS_PENDING forever and this script will time out.
#
# Usage: scripts/test-withdrawal.sh [SIDECHAIN_ID] [PAYOUT_SATS] [FEE_SATS] [DEST_ADDRESS]
#   SIDECHAIN_ID  defaults to 10 (beth's slot in this repo's setup).
#   PAYOUT_SATS   defaults to 10000000 (0.1 BTC).
#   FEE_SATS      defaults to 1000.
#   DEST_ADDRESS  defaults to a fresh address from bitcoind's own "regtest" wallet, so this
#                 script can verify the payout by checking that wallet's received amount.
#
# Needs bitcoin-patched (not stock bitcoin/), like scripts/test-deposit.sh, for BIP300 consensus
# rules (spending the treasury's OP_DRIVECHAIN-tagged output).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DATADIR="${DATADIR:-$ROOT/DATADIR}"

SIDECHAIN_ID="${1:-10}"
PAYOUT_SATS="${2:-10000000}"
FEE_SATS="${3:-1000}"
DEST_ADDRESS="${4:-}"

ENFORCER_GRPC_ADDR="${ENFORCER_GRPC_ADDR:-127.0.0.1:50051}"
BETH_HTTP_ADDR="${BETH_HTTP_ADDR:-127.0.0.1:8545}"
BITCOIN_DATADIR="${BITCOIN_DATADIR:-$DATADIR/bitcoin}"
BITCOIN_CLI="${BITCOIN_CLI:-$ROOT/bitcoin-patched/build/bin/bitcoin-cli}"
MINING_WALLET="${MINING_WALLET:-regtest}"
WALLET_DIR="${WALLET_DIR:-$DATADIR/wallet}"
WALLET_NAME="${WALLET_NAME:-regtest}"
WITHDRAWAL_QUEUE_ADDR="0x000000000000000000000000000000000000B300"
POLL_INTERVAL=2
POLL_TIMEOUT=300 # seconds to wait for STATUS_CONFIRMED once the tx itself is mined

for tool in curl jq; do
    command -v "$tool" >/dev/null || { echo "error: $tool is required" >&2; exit 1; }
done
[ -x "$BITCOIN_CLI" ] || { echo "error: bitcoin-cli not found at $BITCOIN_CLI" >&2; exit 1; }

# Same lookup as scripts/regtest-up.sh and scripts/test-deposit.sh.
find_cast() {
    if command -v cast >/dev/null 2>&1; then command -v cast; return 0; fi
    local candidates=()
    [ -n "${XDG_CONFIG_HOME:-}" ] && candidates+=("$XDG_CONFIG_HOME/.foundry/bin/cast")
    candidates+=("$HOME/.config/.foundry/bin/cast" "$HOME/.foundry/bin/cast")
    local c
    for c in "${candidates[@]}"; do
        [ -x "$c" ] && { printf '%s' "$c"; return 0; }
    done
    return 1
}
CAST="$(find_cast || true)"
if [ -z "$CAST" ] || [ ! -f "$WALLET_DIR/$WALLET_NAME" ]; then
    echo "error: no cast wallet found at $WALLET_DIR/$WALLET_NAME. Run scripts/regtest-up.sh" >&2
    echo "  (it creates the wallet), or scripts/install-foundry.sh if cast itself is missing." >&2
    exit 1
fi
WALLET_ADDR="$("$CAST" wallet address --keystore "$WALLET_DIR/$WALLET_NAME" --password-file "$WALLET_DIR/password.txt")"

bitcoin_cli() { "$BITCOIN_CLI" -datadir="$BITCOIN_DATADIR" -regtest "$@"; }

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

eth_rpc() {
    curl -sS -X POST -H 'Content-Type: application/json' --data "$1" "http://$BETH_HTTP_ADDR"
}

echo "Checking enforcer at $ENFORCER_GRPC_ADDR..."
chain_info="$(rpc ValidatorService/GetChainInfo '{}')"
network="$(jq -r '.network' <<<"$chain_info")"
[ "$network" = "NETWORK_REGTEST" ] || { echo "error: enforcer network is $network, expected NETWORK_REGTEST" >&2; exit 1; }

active="$(rpc ValidatorService/GetSidechains '{}' \
    | jq --argjson id "$SIDECHAIN_ID" '[.sidechains[]? | select(.sidechainNumber == $id)] | length > 0')"
if [ "$active" != "true" ]; then
    echo "error: sidechain $SIDECHAIN_ID is not active. Run scripts/activate-sidechain.sh $SIDECHAIN_ID first." >&2
    exit 1
fi

ctip_value="$(rpc ValidatorService/GetCtip "$(jq -n --argjson id "$SIDECHAIN_ID" '{sidechainNumber: $id}')" | jq -r '.ctip.value // 0')"
needed=$((PAYOUT_SATS + FEE_SATS))
if [ "$ctip_value" -lt "$needed" ]; then
    echo "error: sidechain $SIDECHAIN_ID treasury only holds $ctip_value sat(s), need $needed (payout + fee)." >&2
    echo "  Run scripts/test-deposit.sh $SIDECHAIN_ID first to fund it." >&2
    exit 1
fi
echo "Sidechain $SIDECHAIN_ID treasury currently holds $ctip_value sat(s)."

echo "Checking beth at $BETH_HTTP_ADDR..."
block1="$(eth_rpc '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' | jq -r '.result // empty')"
[ -n "$block1" ] || { echo "error: could not reach beth at $BETH_HTTP_ADDR" >&2; exit 1; }

echo "Setting ACK-all policy so mined blocks vote on the bundle once broadcast..."
rpc BlockProducerService/SetAckAllProposals '{"ackAll": true}' >/dev/null

bitcoin_cli createwallet "$MINING_WALLET" >/dev/null 2>&1 || bitcoin_cli loadwallet "$MINING_WALLET" >/dev/null 2>&1 || true
if [ -z "$DEST_ADDRESS" ]; then
    DEST_ADDRESS="$(bitcoin_cli -rpcwallet="$MINING_WALLET" getnewaddress)"
fi
dest_spk="$(bitcoin_cli getaddressinfo "$DEST_ADDRESS" | jq -r .scriptPubKey)"
[ -n "$dest_spk" ] && [ "$dest_spk" != "null" ] || { echo "error: could not resolve scriptPubKey for $DEST_ADDRESS" >&2; exit 1; }

# beth values ETH at 1e10 wei/sat (see beth/contracts/WithdrawalRequestQueue.sol) -- msg.value
# must equal (valueSats + mainFeeSats) * 1e10 exactly, or the call reverts.
wei=$(((PAYOUT_SATS + FEE_SATS) * 10000000000))
balance_wei="$(eth_rpc "$(jq -n --arg a "$WALLET_ADDR" '{jsonrpc:"2.0",id:1,method:"eth_getBalance",params:[$a,"latest"]}')" | jq -r '.result // "0x0"')"
if [ "$((balance_wei))" -lt "$wei" ]; then
    echo "error: cast wallet $WALLET_ADDR only holds $((balance_wei)) wei, need at least $wei" >&2
    echo "  (plus gas). Run scripts/test-deposit.sh $SIDECHAIN_ID first to fund it." >&2
    exit 1
fi

echo "Requesting withdrawal: $PAYOUT_SATS sats (fee $FEE_SATS sats) to $DEST_ADDRESS"
echo "  (scriptPubKey 0x$dest_spk), from $WALLET_ADDR..."
request_count_before="$("$CAST" call --rpc-url "http://$BETH_HTTP_ADDR" "$WITHDRAWAL_QUEUE_ADDR" 'requestCount()(uint256)')"

echo "Sending requestWithdrawal tx (this blocks until beth mines it -- make sure"
echo "  scripts/bmm-mine.sh $SIDECHAIN_ID is running in another terminal)..."
tx_hash="$("$CAST" send \
    --keystore "$WALLET_DIR/$WALLET_NAME" --password-file "$WALLET_DIR/password.txt" \
    --rpc-url "http://$BETH_HTTP_ADDR" \
    "$WITHDRAWAL_QUEUE_ADDR" 'requestWithdrawal(bytes,uint256)' "0x$dest_spk" "$FEE_SATS" \
    --value "$wei" --json | jq -r '.transactionHash')"
[ -n "$tx_hash" ] && [ "$tx_hash" != "null" ] || { echo "error: cast send did not return a transaction hash" >&2; exit 1; }
echo "Confirmed on beth: $tx_hash"

request_index="$request_count_before"
echo "Assigned request index: $request_index"

echo
echo "Polling WithdrawalRequestQueue.requests($request_index).status (PENDING=0, BUNDLED=1,"
echo "  CONFIRMED=2, REFUNDED=3) -- needs bmm-mine.sh actively mining to progress..."
last_status=""
outcome=""
elapsed=0
while [ "$elapsed" -lt "$POLL_TIMEOUT" ]; do
    request="$("$CAST" call --rpc-url "http://$BETH_HTTP_ADDR" "$WITHDRAWAL_QUEUE_ADDR" \
        'requests(uint256)(address,uint256,uint256,bytes,uint8,bytes32)' "$request_index")"
    status="$(awk 'NR==5' <<<"$request" | tr -dc '0-9')"
    if [ "$status" != "$last_status" ]; then
        echo "  [$elapsed s] status = $status"
        last_status="$status"
    fi
    case "$status" in
    2) outcome="confirmed"; break ;;
    3) outcome="refunded"; break ;;
    esac
    sleep "$POLL_INTERVAL"
    elapsed=$((elapsed + POLL_INTERVAL))
done

case "$outcome" in
confirmed)
    echo "Bundle confirmed: request $request_index is STATUS_CONFIRMED."
    ;;
refunded)
    echo "error: request $request_index was refunded (bundle expired or failed) instead of confirmed." >&2
    exit 1
    ;;
*)
    echo "error: request $request_index did not confirm within ${POLL_TIMEOUT}s (last status: $last_status)." >&2
    echo "  Is scripts/bmm-mine.sh $SIDECHAIN_ID running against this same stack?" >&2
    exit 1
    ;;
esac

echo
echo "Verifying $DEST_ADDRESS actually received the payout on L1..."
received="$(bitcoin_cli -rpcwallet="$MINING_WALLET" getreceivedbyaddress "$DEST_ADDRESS" 0)"
received_sats=$(awk -v b="$received" 'BEGIN { printf "%d", (b * 100000000) + 0.5 }')
if [ "$received_sats" -eq "$PAYOUT_SATS" ]; then
    echo "Confirmed: $DEST_ADDRESS received exactly $PAYOUT_SATS sat(s) ($received BTC)."
else
    echo "warning: $DEST_ADDRESS shows $received_sats sat(s) received, expected $PAYOUT_SATS." >&2
    exit 1
fi

new_ctip="$(rpc ValidatorService/GetCtip "$(jq -n --argjson id "$SIDECHAIN_ID" '{sidechainNumber: $id}')" | jq -r '.ctip.value // 0')"
echo "Sidechain $SIDECHAIN_ID treasury now holds $new_ctip sat(s) (was $ctip_value)."
