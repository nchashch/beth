#!/usr/bin/env bash
# Tests a BIP300 deposit (Bitcoin mainchain -> sidechain) against a running bip300301_enforcer
# + bitcoind regtest stack (see scripts/regtest-up.sh), for a sidechain slot already activated
# via scripts/activate-sidechain.sh.
#
# Usage: scripts/test-deposit.sh [SIDECHAIN_ID] [AMOUNT_SATS] [FEE_SATS] [RECIPIENT]
#   SIDECHAIN_ID defaults to 10 (beth's slot in this repo's setup).
#   AMOUNT_SATS  defaults to 100000000 (1 BTC == 1e18 wei on beth, at its 1e10 wei/sat rate).
#   FEE_SATS     defaults to 1000.
#   RECIPIENT    defaults to the address of the cast wallet regtest-up.sh sets up in
#                $DATADIR/wallet. Must be a standard 20-byte "0x"-prefixed Ethereum address --
#                beth reads a deposit's `address` field as UTF-8 text and parses it the same way
#                any other Ethereum tool would (see beth/src/enforcer.rs's
#                `parse_deposit_address`), so this is just the address's ordinary string form,
#                no special encoding needed.
#
# What this proves: that a deposit transaction can be built, broadcast, mined, and is recognized
# by the enforcer as a BIP300 deposit event for the target sidechain (mirrors the `deposit()`
# helper in bip300301_enforcer's own integration tests) -- and, if scripts/bmm-mine.sh (or
# anything else) has driven beth's block production since, that the recipient's L2 balance
# actually reflects it, checked here via a real `eth_getBalance` call.
#
# Each deposit after a sidechain slot's first must spend the previous one's OP_DRIVECHAIN-tagged
# treasury output to carry the running balance forward (its "CTIP") -- this requires bitcoind to
# actually implement BIP300 consensus rules. regtest-up.sh points at bitcoin-patched/ (LayerTwo-
# Labs' drivechain fork) for exactly this reason; against stock/unpatched Bitcoin Core, every
# deposit past the first fails with `mempool-script-verify-flag-failed (NOPx reserved for
# soft-fork upgrades)` (see bitcoin-patched/README.md and bip300301_enforcer's own integration
# tests, which explicitly skip `deposit_withdraw_roundtrip` for unpatched/stock bitcoind).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DATADIR="${DATADIR:-$ROOT/DATADIR}"

SIDECHAIN_ID="${1:-10}"
AMOUNT_SATS="${2:-100000000}"
FEE_SATS="${3:-1000}"
RECIPIENT="${4:-}"

ENFORCER_GRPC_ADDR="${ENFORCER_GRPC_ADDR:-127.0.0.1:50051}"
BETH_HTTP_ADDR="${BETH_HTTP_ADDR:-127.0.0.1:8545}"
BITCOIN_DATADIR="${BITCOIN_DATADIR:-$DATADIR/bitcoin}"
BITCOIN_CLI="${BITCOIN_CLI:-$ROOT/bitcoin-patched/build/bin/bitcoin-cli}"
MINING_WALLET="${MINING_WALLET:-regtest}"
WALLET_DIR="${WALLET_DIR:-$DATADIR/wallet}"
WALLET_NAME="${WALLET_NAME:-regtest}"
FUND_BLOCKS=100 # coinbase maturity, so the enforcer's wallet can actually spend the funding UTXO

for tool in curl jq od; do
    command -v "$tool" >/dev/null || { echo "error: $tool is required" >&2; exit 1; }
done
[ -x "$BITCOIN_CLI" ] || { echo "error: bitcoin-cli not found at $BITCOIN_CLI" >&2; exit 1; }

# Same lookup as scripts/regtest-up.sh: foundryup's installer respects XDG_CONFIG_HOME when set,
# installing to $XDG_CONFIG_HOME/.foundry rather than $HOME/.foundry.
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

if [ -z "$RECIPIENT" ]; then
    CAST="$(find_cast || true)"
    if [ -z "$CAST" ] || [ ! -f "$WALLET_DIR/$WALLET_NAME" ]; then
        echo "error: no RECIPIENT given, and no cast wallet found at $WALLET_DIR/$WALLET_NAME to" >&2
        echo "  default to. Run scripts/regtest-up.sh (it creates the wallet), or pass an" >&2
        echo "  address explicitly." >&2
        exit 1
    fi
    RECIPIENT="$("$CAST" wallet address --keystore "$WALLET_DIR/$WALLET_NAME" --password-file "$WALLET_DIR/password.txt")"
fi

if ! [[ "$RECIPIENT" =~ ^0x[0-9a-fA-F]{40}$ ]]; then
    echo "error: RECIPIENT must be a standard 20-byte \"0x\"-prefixed Ethereum address, got \"$RECIPIENT\"" >&2
    exit 1
fi
# What the enforcer actually stores/reports on-chain is the hex of the address *string's* own
# ASCII bytes (42 of them, "0x" included) -- not the 20 address bytes it represents. See
# beth/src/enforcer.rs's parse_deposit_address for the two-step decode this mirrors.
recipient_wire_hex="$(printf '%s' "$RECIPIENT" | od -An -tx1 | tr -d ' \n')"

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

echo "Funding the enforcer's wallet ($FUND_BLOCKS blocks, to mature a coinbase it can spend)..."
fund_addr="$(rpc WalletService/CreateNewAddress '{}' | jq -r '.address')"
bitcoin_cli generatetoaddress "$FUND_BLOCKS" "$fund_addr" >/dev/null

echo "Creating deposit: $AMOUNT_SATS sats (fee $FEE_SATS sats) to sidechain $SIDECHAIN_ID,"
echo "  recipient $RECIPIENT..."
deposit_req="$(jq -n \
    --argjson id "$SIDECHAIN_ID" \
    --arg addr "$RECIPIENT" \
    --argjson val "$AMOUNT_SATS" \
    --argjson fee "$FEE_SATS" \
    '{sidechainId: $id, address: $addr, valueSats: $val, feeSats: $fee}')"

# The enforcer's wallet needs to have synced the funding UTXO above before it can spend it;
# that sync runs on a background timer, so retry instead of assuming it's already caught up.
err_file="$(mktemp)"
trap 'rm -f "$err_file"' EXIT
txid=""
waited=0
while [ -z "$txid" ]; do
    if resp="$(rpc WalletService/CreateDepositTransaction "$deposit_req" 2>"$err_file")"; then
        txid="$(jq -r '.txid.hex // empty' <<<"$resp")"
        [ -n "$txid" ] || { echo "error: CreateDepositTransaction response had no txid: $resp" >&2; exit 1; }
    else
        waited=$((waited + 3))
        if [ "$waited" -ge 90 ]; then
            echo "error: CreateDepositTransaction kept failing after ${waited}s. Last error:" >&2
            cat "$err_file" >&2
            exit 1
        fi
        sleep 3
    fi
done
echo "Deposit txid: $txid"

echo "Waiting for the deposit tx to enter bitcoind's mempool..."
waited=0
until bitcoin_cli getmempoolentry "$txid" >/dev/null 2>&1; do
    sleep 1
    waited=$((waited + 1))
    if [ "$waited" -ge 30 ]; then
        echo "error: tx $txid never appeared in the mempool" >&2
        exit 1
    fi
done

echo "Mining 1 block to confirm the deposit..."
bitcoin_cli createwallet "$MINING_WALLET" >/dev/null 2>&1 || bitcoin_cli loadwallet "$MINING_WALLET" >/dev/null 2>&1 || true
mining_addr="$(bitcoin_cli -rpcwallet="$MINING_WALLET" getnewaddress)"
confirming_block="$(bitcoin_cli generatetoaddress 1 "$mining_addr" | jq -r '.[0]')"

echo "Checking the enforcer recognized the deposit..."
# The enforcer ingests new blocks asynchronously (via ZMQ), so its own reported tip can briefly
# lag bitcoind's right after mining -- poll GetChainTip until it has caught up to our block's
# height. This checks height, not an exact tip-hash match: something else (e.g.
# scripts/bmm-mine.sh, mining through the enforcer once a second) may keep extending the chain
# past our specific confirming_block in the meantime, which an exact-hash wait would never see.
confirming_height="$(bitcoin_cli getblockheader "$confirming_block" | jq -r '.height')"
waited=0
while [ "$(rpc ValidatorService/GetChainTip '{}' | jq -r '.blockHeaderInfo.height')" -lt "$confirming_height" ]; do
    sleep 1
    waited=$((waited + 1))
    if [ "$waited" -ge 30 ]; then
        echo "error: enforcer never caught up to block $confirming_block (height $confirming_height)" >&2
        exit 1
    fi
done

# Query up to whatever the enforcer's tip actually is now (which may be past confirming_block) --
# GetTwoWayPegData reports full accumulated history up to endBlockHash, so the deposit event is
# still in there regardless.
enforcer_tip="$(rpc ValidatorService/GetChainTip '{}' | jq -r '.blockHeaderInfo.blockHash.hex')"
peg_data_req="$(jq -n --argjson id "$SIDECHAIN_ID" --arg tip "$enforcer_tip" '{sidechainId: $id, endBlockHash: {hex: $tip}}')"
peg_data="$(rpc ValidatorService/GetTwoWayPegData "$peg_data_req")"
matched="$(jq --arg hex "$recipient_wire_hex" --argjson val "$AMOUNT_SATS" '
    [.blocks[]?.blockInfo.events[]? | select(has("deposit")) | .deposit.output
     | select(.address.hex == $hex and (.valueSats | tonumber) == $val)] | length > 0
' <<<"$peg_data")"

if [ "$matched" = "true" ]; then
    echo "Confirmed: the enforcer recognized a $AMOUNT_SATS-sat deposit to $RECIPIENT on sidechain $SIDECHAIN_ID."
else
    echo "warning: no matching deposit event found in GetTwoWayPegData. Response:" >&2
    jq . <<<"$peg_data" >&2
    exit 1
fi

n_deposits="$(rpc WalletService/ListSidechainDepositTransactions '{}' | jq '.transactions | length')"
echo "WalletService.ListSidechainDepositTransactions now lists $n_deposits deposit(s) total."

echo
echo "beth-side balance check for $RECIPIENT:"
balance_req="$(jq -n --arg addr "$RECIPIENT" '{jsonrpc: "2.0", method: "eth_getBalance", params: [$addr, "latest"], id: 1}')"
balance_resp="$(curl -s -X POST -H 'Content-Type: application/json' --data "$balance_req" "http://$BETH_HTTP_ADDR")"
balance_hex="$(jq -r '.result // empty' <<<"$balance_resp")"
if [ -z "$balance_hex" ]; then
    echo "  could not reach beth at $BETH_HTTP_ADDR ($balance_resp)"
else
    echo "  eth_getBalance($RECIPIENT) = $balance_hex"
    if [ "$balance_hex" = "0x0" ]; then
        cat <<EOF
  0 is expected here unless scripts/bmm-mine.sh (or something else) has driven beth's block
  production since this deposit landed: the deposit is now committed on L1 and visible to the
  enforcer, but crediting it into beth's EVM state happens via DepositVault.systemCreditDeposits,
  which only runs when beth builds or imports its next block.
EOF
    fi
fi
