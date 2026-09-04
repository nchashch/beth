#!/usr/bin/env bash
# Drives beth's block production via BIP301 blind-merge-mining (BMM), once per second, in a
# loop. Nothing else in this repo does this (see the note at the end of scripts/regtest-up.sh) --
# without it, beth never imports a new block, so no transaction (a transfer, a deployed
# contract, anything) ever confirms.
#
# Requires: the regtest stack up (scripts/regtest-up.sh) and the target sidechain slot already
# active (scripts/activate-sidechain.sh $SIDECHAIN_ID).
#
# Safe to stop (Ctrl-C) and re-run: it tracks its own last-mined head in
# $DATADIR/beth/.bmm-head and resumes from there, rather than restarting from genesis each time.
#
# Expect a burst of "BMM bid rejected: Insufficient funds" lines for roughly the first minute
# after a fresh start: the enforcer's wallet syncs via periodic Electrum polling (not
# instantly), so it takes a little while to notice the funding transfer this script just made,
# even though the coinbase itself matured immediately. The loop self-heals once it does --
# this isn't a stall, no intervention needed.
#
# Usage: scripts/bmm-mine.sh [SIDECHAIN_ID]   (defaults to 10, beth's slot in this repo's setup)
#
# Each iteration:
#   1. engine_forkchoiceUpdatedV3 (JWT-authenticated) with payload attributes -- starts a beth
#      payload build on top of the current head. Its extraData gets stamped with the current
#      BIP301 mainchain tip by beth's own payload builder (see beth/src/payload.rs).
#   2. engine_getPayloadV4 -- retrieves the built block. Its hash is H*.
#   3. WalletService.CreateBmmCriticalDataTransaction -- bids H* against that same mainchain tip.
#   4. MiningService.GenerateToAddress -- mines ONE L1 block *through the enforcer*. This is
#      what actually embeds the BMM-accept message (M7) into that block's coinbase; a block
#      mined via plain bitcoin-cli would not carry it (see scripts/test-withdrawal.sh's header
#      comment for the same gotcha with mining withdrawal-bundle votes).
#   5. engine_newPayloadV4 -- submits the block. beth's consensus check (fixed in
#      beth/src/enforcer.rs's `is_committed`) searches the mainchain tip and its recent
#      ancestors for the commitment, since the enforcer embeds it in the *next* mainchain block
#      after the bid, never in the referenced tip itself.
#   6. engine_forkchoiceUpdatedV3 (head-only) -- advances the canonical head to the new block.
#
# Sleeps 1s between iterations. A failed iteration (e.g. the L1 side isn't ready yet) is logged
# and skipped rather than stopping the loop.

set -uo pipefail # not -e: a single bad iteration shouldn't kill the loop

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DATADIR="${DATADIR:-$ROOT/DATADIR}"

SIDECHAIN_ID="${1:-10}"
BID_SATS="${BID_SATS:-1000}"

BETH_HTTP_ADDR="${BETH_HTTP_ADDR:-127.0.0.1:8545}"
BETH_AUTHRPC_ADDR="${BETH_AUTHRPC_ADDR:-127.0.0.1:8551}"
ENFORCER_GRPC_ADDR="${ENFORCER_GRPC_ADDR:-127.0.0.1:50051}"
JWT_SECRET_FILE="${JWT_SECRET_FILE:-$DATADIR/beth/jwt.hex}"
MINING_WALLET="${MINING_WALLET:-regtest}"
BITCOIN_DATADIR="${BITCOIN_DATADIR:-$DATADIR/bitcoin}"
BITCOIN_CLI="${BITCOIN_CLI:-$ROOT/bitcoin-patched/build/bin/bitcoin-cli}"

for tool in curl jq openssl; do
    command -v "$tool" >/dev/null || { echo "error: $tool is required" >&2; exit 1; }
done
[ -f "$JWT_SECRET_FILE" ] || { echo "error: no JWT secret at $JWT_SECRET_FILE (is beth running?)" >&2; exit 1; }
[ -x "$BITCOIN_CLI" ] || { echo "error: bitcoin-cli not found at $BITCOIN_CLI" >&2; exit 1; }

JWT_SECRET="$(tr -d '\n' <"$JWT_SECRET_FILE" | sed 's/^0x//')"

b64url() { base64 -w0 | tr '+/' '-_' | tr -d '='; }

# A fresh HS256 JWT per call -- reth checks `iat` is recent, so these can't be reused/cached.
make_jwt() {
    local header='{"alg":"HS256","typ":"JWT"}'
    local claims
    claims="$(printf '{"iat":%d}' "$(date +%s)")"
    local h p sig
    h="$(printf '%s' "$header" | b64url)"
    p="$(printf '%s' "$claims" | b64url)"
    sig="$(printf '%s.%s' "$h" "$p" | openssl dgst -sha256 -mac HMAC -macopt hexkey:"$JWT_SECRET" -binary | b64url)"
    printf '%s.%s.%s' "$h" "$p" "$sig"
}

engine_rpc() {
    curl -sS -X POST -H 'Content-Type: application/json' -H "Authorization: Bearer $(make_jwt)" \
        --data "$1" "http://$BETH_AUTHRPC_ADDR"
}

mainchain_rpc() {
    curl -sS -X POST -H 'Content-Type: application/json' --data "$2" \
        "http://$ENFORCER_GRPC_ADDR/cusf.mainchain.v1.$1"
}

bitcoin_cli() { "$BITCOIN_CLI" -datadir="$BITCOIN_DATADIR" -regtest "$@"; }

bitcoin_cli createwallet "$MINING_WALLET" >/dev/null 2>&1 || bitcoin_cli loadwallet "$MINING_WALLET" >/dev/null 2>&1 || true

active="$(mainchain_rpc ValidatorService/GetSidechains '{}' \
    | jq --argjson id "$SIDECHAIN_ID" '[.sidechains[]? | select(.sidechainNumber == $id)] | length > 0')"
if [ "$active" != "true" ]; then
    echo "error: sidechain $SIDECHAIN_ID is not active. Run scripts/activate-sidechain.sh $SIDECHAIN_ID first." >&2
    exit 1
fi

# The enforcer's own wallet pays each bid's fee+value; fund it generously up front (cheap and
# instant on regtest) so the loop doesn't stall partway through on insufficient funds.
fund_addr="$(mainchain_rpc WalletService/CreateNewAddress '{}' | jq -r '.address')"
bitcoin_cli generatetoaddress 100 "$fund_addr" >/dev/null
echo "Funded the enforcer's wallet."

# beth's `eth_getBlockByNumber`/`eth_getBlockByHash` can return null for a block just mined
# moments ago even though reth's own logs confirm it's canonical (some indexing lag) -- so this
# can't just look up "latest" every iteration to track where we are. Instead, track our own
# last-known head in a file inside beth's datadir, so it naturally resets together with the
# chain (wiped by scripts/regtest-reset.sh along with everything else under $DATADIR/beth)
# rather than going stale across a reset.
HEAD_FILE="$DATADIR/beth/.bmm-head"
GENESIS="$(curl -sS -X POST -H 'Content-Type: application/json' \
    --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["earliest",false]}' \
    "http://$BETH_HTTP_ADDR" | jq -r '.result.hash')"
[ -n "$GENESIS" ] && [ "$GENESIS" != "null" ] || { echo "error: could not read beth's genesis hash -- is it running?" >&2; exit 1; }

validate_head() {
    # Confirm beth still actually knows this block before trusting it as a starting point -- a
    # head-only forkchoiceUpdated on a known block returns VALID; on an unknown one, SYNCING.
    jq -r '.result.payloadStatus.status // empty' <<<"$(engine_rpc "$(jq -n --arg h "$1" \
        '{jsonrpc:"2.0",id:1,method:"engine_forkchoiceUpdatedV3",params:[{headBlockHash:$h,safeBlockHash:$h,finalizedBlockHash:$h}, null]}')")"
}

HEAD=""
if [ -s "$HEAD_FILE" ]; then
    candidate="$(cat "$HEAD_FILE")"
    if [ "$(validate_head "$candidate")" = "VALID" ]; then
        HEAD="$candidate"
        echo "Resuming from previously-tracked beth head $HEAD"
    else
        echo "Tracked head $candidate is no longer known to beth -- falling back."
    fi
fi
if [ -z "$HEAD" ]; then
    # No tracked head yet (first run since this script started tracking one, or a fresh chain) --
    # one-time fallback to whatever beth itself reports as its current tip, in case blocks were
    # already mined some other way (e.g. an earlier run of this same script, before it tracked a
    # head file). "latest" is reliable enough for this one-off startup check, just not to poll
    # every iteration.
    latest="$(curl -sS -X POST -H 'Content-Type: application/json' \
        --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["latest",false]}' \
        "http://$BETH_HTTP_ADDR" | jq -r '.result.hash // empty')"
    if [ -n "$latest" ] && [ "$latest" != "null" ] && [ "$(validate_head "$latest")" = "VALID" ]; then
        HEAD="$latest"
        echo "No tracked head found; resuming from beth's current tip $HEAD"
    fi
fi
if [ -z "$HEAD" ]; then
    HEAD="$GENESIS"
    echo "Starting from beth genesis $HEAD"
fi

trap 'echo; echo "Stopped after $((n)) block(s)."; exit 0' INT TERM

n=0
while true; do
    n=$((n + 1))

    ts="$(printf '0x%x' $(($(date +%s) + 1)))"
    fcu_build_req="$(jq -n --arg h "$HEAD" --arg ts "$ts" '{
        jsonrpc: "2.0", id: 1, method: "engine_forkchoiceUpdatedV3",
        params: [
            {headBlockHash: $h, safeBlockHash: $h, finalizedBlockHash: $h},
            {timestamp: $ts, prevRandao: "0x0000000000000000000000000000000000000000000000000000000000000000",
             suggestedFeeRecipient: "0x0000000000000000000000000000000000000001", withdrawals: [],
             parentBeaconBlockRoot: "0x0000000000000000000000000000000000000000000000000000000000000000"}
        ]}')"
    fcu_build_resp="$(engine_rpc "$fcu_build_req")"
    payload_id="$(jq -r '.result.payloadId // empty' <<<"$fcu_build_resp")"
    if [ -z "$payload_id" ]; then
        echo "[$n] skip: forkchoiceUpdated didn't return a payloadId: $fcu_build_resp" >&2
        sleep 1
        continue
    fi

    # If getPayload races ahead of the background build job's first pass, beth falls back to
    # `build_empty_payload` -- which is *always* empty, by design (see beth/src/payload.rs) --
    # so a pending mempool transaction never gets included no matter how many blocks get mined
    # (confirmed empirically: 0.5s wasn't enough, 3s was). Only wait when there's actually
    # something pending, so idle blocks (the common case) still build at close to once a second.
    pool_pending="$(curl -sS -X POST -H 'Content-Type: application/json' \
        --data '{"jsonrpc":"2.0","id":1,"method":"txpool_status","params":[]}' \
        "http://$BETH_HTTP_ADDR" | jq -r '.result.pending // "0x0"')"
    [ "$pool_pending" != "0x0" ] && sleep 2

    payload_resp="$(engine_rpc "$(jq -n --arg pid "$payload_id" '{jsonrpc:"2.0",id:1,method:"engine_getPayloadV4",params:[$pid]}')")"
    execution_payload="$(jq -c '.result.executionPayload' <<<"$payload_resp")"
    execution_requests="$(jq -c '.result.executionRequests // []' <<<"$payload_resp")"
    block_hash="$(jq -r '.blockHash // empty' <<<"$execution_payload")"
    extra_data="$(jq -r '.extraData // empty' <<<"$execution_payload")"
    if [ -z "$block_hash" ] || [ -z "$extra_data" ]; then
        echo "[$n] skip: no payload retrieved: $payload_resp" >&2
        continue
    fi
    critical_hash="${block_hash#0x}"
    prev_bytes="${extra_data#0x}"

    bid_req="$(jq -n --argjson id "$SIDECHAIN_ID" --argjson sats "$BID_SATS" --arg ch "$critical_hash" --arg pb "$prev_bytes" \
        '{sidechainId: $id, valueSats: $sats, height: 0, criticalHash: {hex: $ch}, prevBytes: {hex: $pb}}')"
    bid_resp="$(mainchain_rpc WalletService/CreateBmmCriticalDataTransaction "$bid_req")"
    bid_code="$(jq -r '.code // empty' <<<"$bid_resp")"
    if [ -n "$bid_code" ] && [ "$bid_code" != "already_exists" ]; then
        echo "[$n] skip: BMM bid rejected (mainchain tip probably moved): $bid_resp" >&2
        continue
    fi
    # "already_exists" means the enforcer already has a bid outstanding for this exact
    # (sidechain, mainchain tip) pair -- from an earlier iteration whose own newPayload/FCU steps
    # below failed after the bid succeeded, or from the enforcer's own internal retry. Either way
    # there's still something pending worth trying to confirm, so fall through to mining instead
    # of giving up: previously this `continue`d unconditionally, which meant a single stuck
    # unconfirmed bid caused every subsequent iteration to bail out here forever without ever
    # calling GenerateToAddress again -- a livelock that was only discovered when the resulting
    # mempool backlog (hundreds of never-mined bid transactions, each iteration adding one more)
    # eventually bogged the enforcer's own wallet/validator down badly enough to fall behind
    # bitcoind entirely.

    mine_addr="$(bitcoin_cli -rpcwallet="$MINING_WALLET" getnewaddress)"
    mine_resp="$(mainchain_rpc MiningService/GenerateToAddress "$(jq -n --arg a "$mine_addr" '{blocks: 1, address: $a}')")"
    confirming_block="$(jq -r '.blockHashes[0].hex // empty' <<<"$mine_resp")"
    if [ -z "$confirming_block" ]; then
        echo "[$n] skip: mining the confirming block failed: $mine_resp" >&2
        continue
    fi

    new_payload_req="$(jq -n --argjson p "$execution_payload" --argjson r "$execution_requests" '{
        jsonrpc: "2.0", id: 1, method: "engine_newPayloadV4",
        params: [$p, [], "0x0000000000000000000000000000000000000000000000000000000000000000", $r]
    }')"
    new_payload_resp="$(engine_rpc "$new_payload_req")"
    status="$(jq -r '.result.status // empty' <<<"$new_payload_resp")"
    if [ "$status" != "VALID" ]; then
        echo "[$n] rejected: $(jq -c '.result // .error' <<<"$new_payload_resp")" >&2
        continue
    fi

    fcu_final_req="$(jq -n --arg h "$block_hash" '{jsonrpc:"2.0",id:1,method:"engine_forkchoiceUpdatedV3",params:[{headBlockHash:$h,safeBlockHash:$h,finalizedBlockHash:$h}, null]}')"
    engine_rpc "$fcu_final_req" >/dev/null

    HEAD="$block_hash"
    printf '%s' "$HEAD" >"$HEAD_FILE"
    echo "[$n] beth block $HEAD (BMM-committed via mainchain block $confirming_block)"

    sleep 1
done
