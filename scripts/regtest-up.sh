#!/usr/bin/env bash
# Brings up the local BIP300/301 regtest stack: bitcoind -> electrs -> bip300301_enforcer,
# with beth (the reth-based sidechain) wired to the enforcer.
#
#   bitcoind (regtest, cookie auth)
#     |-- ZMQ (rawblock/rawtx/sequence) --> bip300301_enforcer
#     |-- RPC (cookie file)             --> bip300301_enforcer
#     '-- RPC (cookie file)             --> electrs
#   electrs --electrum--> bip300301_enforcer's wallet
#   bip300301_enforcer --gRPC (Connect)--> beth
#
# All four run in the foreground under this script; Ctrl-C (or any of them dying) tears
# down the rest via the trap below. Logs and PIDs land under $DATADIR/logs.
#
# Requires the binaries to already be built (see the *_BIN checks below for exact paths
# and build commands). Note this only gets the four processes running and talking to each
# other -- it does NOT drive BIP301 blind-merge-mining (sidechain proposal/ack/activation
# and the actual BMM mining loop are a separate, manual step against the enforcer's
# WalletService).

set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DATADIR="${DATADIR:-$ROOT/DATADIR}"

BITCOIN_DATADIR="$DATADIR/bitcoin"
ENFORCER_DATADIR="$DATADIR/enforcer"
ELECTRS_DATADIR="$DATADIR/electrs"
BETH_DATADIR="$DATADIR/beth"
LOGDIR="$DATADIR/logs"

# Bitcoin Core RPC/ZMQ
BITCOIND_RPC_PORT="${BITCOIND_RPC_PORT:-18443}"
ZMQ_BLOCK_PORT="${ZMQ_BLOCK_PORT:-28332}"
ZMQ_TX_PORT="${ZMQ_TX_PORT:-28333}"
ZMQ_SEQUENCE_PORT="${ZMQ_SEQUENCE_PORT:-28334}"

# bip300301_enforcer
ENFORCER_RPC_ADDR="${ENFORCER_RPC_ADDR:-127.0.0.1:8122}"   # getblocktemplate etc.
ENFORCER_GRPC_ADDR="${ENFORCER_GRPC_ADDR:-127.0.0.1:50051}" # Connect/gRPC; beth talks to this

# electrs
ELECTRS_RPC_PORT="${ELECTRS_RPC_PORT:-60401}"    # electrum protocol (regtest default)
ELECTRS_HTTP_PORT="${ELECTRS_HTTP_PORT:-3002}"    # esplora-style REST (regtest default)

# beth (standard reth ports)
BETH_HTTP_PORT="${BETH_HTTP_PORT:-8545}"
BETH_WS_PORT="${BETH_WS_PORT:-8546}"
BETH_AUTHRPC_PORT="${BETH_AUTHRPC_PORT:-8551}"
BETH_P2P_PORT="${BETH_P2P_PORT:-30303}"
BETH_SIDECHAIN_ID="${BETH_SIDECHAIN_ID:-10}"

# Mine a spendable coinbase on first run so the stack isn't sitting on an empty chain.
AUTO_MINE="${AUTO_MINE:-1}"

# ---------------------------------------------------------------------------
# Binaries -- prefer a release build, fall back to debug, else fail with the build command.
# ---------------------------------------------------------------------------

pick_bin() {
    local name="$1" release="$2" debug="$3"
    if [ -x "$release" ]; then echo "$release";
    elif [ -x "$debug" ]; then echo "$debug";
    else
        echo "error: $name binary not found (looked for $release and $debug)" >&2
        return 1
    fi
}

# bitcoin-patched (LayerTwo-Labs' drivechain fork) rather than stock bitcoin/: only it actually
# implements OP_DRIVECHAIN/BIP300 consensus rules, needed to spend a sidechain's treasury UTXO
# (i.e. for more than one deposit -- see scripts/test-deposit.sh's header comment).
BITCOIND="$ROOT/bitcoin-patched/build/bin/bitcoind"
BITCOIN_CLI="$ROOT/bitcoin-patched/build/bin/bitcoin-cli"
[ -x "$BITCOIND" ] || { echo "error: bitcoind not found at $BITCOIND (build bitcoin-patched/ first)" >&2; exit 1; }
[ -x "$BITCOIN_CLI" ] || { echo "error: bitcoin-cli not found at $BITCOIN_CLI" >&2; exit 1; }

# bip300301_enforcer is a git submodule of beth itself (see beth/.gitmodules, beth/build.rs),
# not a sibling checkout -- pinned to a specific, reviewed commit, the same way thunder-rust
# vendors it.
ENFORCER="$(pick_bin bip300301_enforcer \
    "$ROOT/beth/bip300301_enforcer/target/release/bip300301_enforcer" \
    "$ROOT/beth/bip300301_enforcer/target/debug/bip300301_enforcer")" \
    || { echo "  build with: (cd beth/bip300301_enforcer && cargo build --release), after" >&2
         echo "  'git submodule update --init' inside beth/ if that directory is empty" >&2
         exit 1; }

ELECTRS="$(pick_bin electrs \
    "$ROOT/electrs/target/release/electrs" \
    "$ROOT/electrs/target/debug/electrs")" \
    || { echo "  build with: (cd electrs && cargo build --release --bin electrs)" >&2; exit 1; }

BETH="$(pick_bin beth \
    "$ROOT/beth/target/release/beth" \
    "$ROOT/beth/target/debug/beth")" \
    || { echo "  build with: (cd beth && cargo build --release)" >&2; exit 1; }

echo "Using binaries:"
echo "  bitcoind   = $BITCOIND"
echo "  bitcoin-cli= $BITCOIN_CLI"
echo "  enforcer   = $ENFORCER"
echo "  electrs    = $ELECTRS"
echo "  beth       = $BETH"

# ---------------------------------------------------------------------------
# Directories + bitcoin.conf (cookie auth: no rpcuser/rpcpassword set anywhere)
# ---------------------------------------------------------------------------

mkdir -p "$BITCOIN_DATADIR" "$ENFORCER_DATADIR" "$ELECTRS_DATADIR" "$BETH_DATADIR" "$LOGDIR"

BITCOIN_CONF="$BITCOIN_DATADIR/bitcoin.conf"
if [ ! -f "$BITCOIN_CONF" ]; then
    cat > "$BITCOIN_CONF" <<EOF
regtest=1

[regtest]
server=1
rest=1
rpcbind=127.0.0.1
rpcallowip=127.0.0.1
rpcport=$BITCOIND_RPC_PORT
txindex=1
fallbackfee=0.0001
# BIP300 deposit outputs use an OP_DRIVECHAIN scriptPubKey that this build's mempool policy
# doesn't recognize as standard; regtest-only, harmless to relax here.
acceptnonstdtxn=1

zmqpubhashblock=tcp://127.0.0.1:$ZMQ_BLOCK_PORT
zmqpubrawblock=tcp://127.0.0.1:$ZMQ_BLOCK_PORT

zmqpubhashtx=tcp://127.0.0.1:$ZMQ_TX_PORT
zmqpubrawtx=tcp://127.0.0.1:$ZMQ_TX_PORT

zmqpubsequence=tcp://127.0.0.1:$ZMQ_SEQUENCE_PORT
EOF
    echo "Wrote $BITCOIN_CONF"
fi

COOKIE_FILE="$BITCOIN_DATADIR/regtest/.cookie"

# ---------------------------------------------------------------------------
# Process bookkeeping: track PIDs, kill them all (and their children) on exit.
# ---------------------------------------------------------------------------

PIDS=()

cleanup() {
    trap - EXIT INT TERM
    echo
    echo "Shutting down..."
    for pid in "${PIDS[@]:-}"; do
        [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
    done
    for pid in "${PIDS[@]:-}"; do
        [ -n "$pid" ] && wait "$pid" 2>/dev/null || true
    done
    echo "All processes stopped."
    exit 0
}
trap cleanup EXIT INT TERM

wait_for_port() {
    local host="$1" port="$2" label="$3" timeout="${4:-60}"
    local waited=0
    until (exec 3<>"/dev/tcp/$host/$port") 2>/dev/null; do
        exec 3>&- 2>/dev/null || true
        sleep 1
        waited=$((waited + 1))
        if [ "$waited" -ge "$timeout" ]; then
            echo "error: $label did not open $host:$port within ${timeout}s" >&2
            return 1
        fi
    done
    exec 3>&- 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# 1. bitcoind
# ---------------------------------------------------------------------------

echo
echo "Starting bitcoind (regtest)..."
"$BITCOIND" -datadir="$BITCOIN_DATADIR" -regtest -printtoconsole=0 \
    >"$LOGDIR/bitcoind.log" 2>&1 &
BITCOIND_PID=$!
PIDS+=("$BITCOIND_PID")

wait_for_port 127.0.0.1 "$BITCOIND_RPC_PORT" bitcoind 60

# The cookie file only appears once bitcoind has finished initializing RPC.
waited=0
until [ -f "$COOKIE_FILE" ]; do
    sleep 1
    waited=$((waited + 1))
    if [ "$waited" -ge 60 ]; then
        echo "error: cookie file never appeared at $COOKIE_FILE" >&2
        exit 1
    fi
done

bitcoin_cli() {
    "$BITCOIN_CLI" -datadir="$BITCOIN_DATADIR" -regtest "$@"
}

until bitcoin_cli getblockchaininfo >/dev/null 2>&1; do sleep 1; done
echo "bitcoind is up (cookie: $COOKIE_FILE)"

if [ "$AUTO_MINE" = "1" ]; then
    HEIGHT="$(bitcoin_cli getblockcount)"
    if [ "$HEIGHT" -eq 0 ]; then
        echo "Chain height is 0: creating a wallet and mining 101 blocks so there's a spendable coinbase..."
        bitcoin_cli createwallet regtest >/dev/null 2>&1 || bitcoin_cli loadwallet regtest >/dev/null 2>&1 || true
        MINE_ADDR="$(bitcoin_cli -rpcwallet=regtest getnewaddress)"
        bitcoin_cli generatetoaddress 101 "$MINE_ADDR" >/dev/null
        echo "Mined 101 blocks to $MINE_ADDR"
    fi
fi

# ---------------------------------------------------------------------------
# 2. electrs (indexes bitcoind, cookie auth via --daemon-dir)
# ---------------------------------------------------------------------------

echo
echo "Starting electrs..."
"$ELECTRS" \
    --network regtest \
    --daemon-dir="$BITCOIN_DATADIR" \
    --daemon-rpc-addr="127.0.0.1:$BITCOIND_RPC_PORT" \
    --db-dir="$ELECTRS_DATADIR/db" \
    --electrum-rpc-addr="127.0.0.1:$ELECTRS_RPC_PORT" \
    --http-addr="127.0.0.1:$ELECTRS_HTTP_PORT" \
    --jsonrpc-import \
    >"$LOGDIR/electrs.log" 2>&1 &
ELECTRS_PID=$!
PIDS+=("$ELECTRS_PID")

wait_for_port 127.0.0.1 "$ELECTRS_RPC_PORT" electrs 60
echo "electrs is up (electrum: 127.0.0.1:$ELECTRS_RPC_PORT, http: 127.0.0.1:$ELECTRS_HTTP_PORT)"

# ---------------------------------------------------------------------------
# 3. bip300301_enforcer (cookie auth against bitcoind; wallet synced via electrs)
# ---------------------------------------------------------------------------

echo
echo "Starting bip300301_enforcer..."
"$ENFORCER" \
    --data-dir="$ENFORCER_DATADIR" \
    --node-rpc-addr="127.0.0.1:$BITCOIND_RPC_PORT" \
    --node-rpc-cookie-path="$COOKIE_FILE" \
    --node-zmq-addr-sequence="tcp://127.0.0.1:$ZMQ_SEQUENCE_PORT" \
    --enable-wallet \
    --enable-mempool \
    --wallet-auto-create \
    --wallet-sync-source=electrum \
    --wallet-electrum-host=127.0.0.1 \
    --wallet-electrum-port="$ELECTRS_RPC_PORT" \
    --serve-rpc-addr="$ENFORCER_RPC_ADDR" \
    --serve-grpc-addr="$ENFORCER_GRPC_ADDR" \
    --log-level="${ENFORCER_LOG_LEVEL:-INFO}" \
    >"$LOGDIR/enforcer.log" 2>&1 &
ENFORCER_PID=$!
PIDS+=("$ENFORCER_PID")

ENFORCER_GRPC_HOST="${ENFORCER_GRPC_ADDR%:*}"
ENFORCER_GRPC_PORT="${ENFORCER_GRPC_ADDR##*:}"
wait_for_port "$ENFORCER_GRPC_HOST" "$ENFORCER_GRPC_PORT" bip300301_enforcer 60
echo "bip300301_enforcer is up (rpc: $ENFORCER_RPC_ADDR, grpc: $ENFORCER_GRPC_ADDR)"

# ---------------------------------------------------------------------------
# 4. beth (reth-based sidechain, wired to the enforcer's gRPC)
# ---------------------------------------------------------------------------

echo
echo "Starting beth..."
ENFORCER_URL="http://$ENFORCER_GRPC_ADDR"
BIP300301_ENFORCER_URL="$ENFORCER_URL" \
BETH_SIDECHAIN_ID="$BETH_SIDECHAIN_ID" \
"$BETH" node \
    --chain beth \
    --datadir "$BETH_DATADIR" \
    --disable-discovery \
    --port "$BETH_P2P_PORT" \
    --http \
    --http.addr 127.0.0.1 \
    --http.port "$BETH_HTTP_PORT" \
    --http.api eth,net,web3,txpool \
    --ws \
    --ws.addr 127.0.0.1 \
    --ws.port "$BETH_WS_PORT" \
    --authrpc.port "$BETH_AUTHRPC_PORT" \
    >"$LOGDIR/beth.log" 2>&1 &
BETH_PID=$!
PIDS+=("$BETH_PID")

wait_for_port 127.0.0.1 "$BETH_HTTP_PORT" beth 60
echo "beth is up (http: 127.0.0.1:$BETH_HTTP_PORT, ws: 127.0.0.1:$BETH_WS_PORT), enforcer url=$ENFORCER_URL sidechain_id=$BETH_SIDECHAIN_ID"

# ---------------------------------------------------------------------------
# 5. cast wallet -- for signing transactions against beth. Optional: skipped (not fatal) if
# cast isn't installed (see scripts/install-foundry.sh).
# ---------------------------------------------------------------------------

WALLET_DIR="$DATADIR/wallet"
WALLET_NAME="${WALLET_NAME:-regtest}"
WALLET_KEYSTORE="$WALLET_DIR/$WALLET_NAME"
WALLET_PASSWORD_FILE="$WALLET_DIR/password.txt"

find_cast() {
    if command -v cast >/dev/null 2>&1; then command -v cast; return 0; fi
    # foundryup's installer respects XDG_CONFIG_HOME when set, installing to
    # $XDG_CONFIG_HOME/.foundry rather than $HOME/.foundry -- check both (see
    # scripts/install-foundry.sh's identical logic).
    local candidates=()
    [ -n "${XDG_CONFIG_HOME:-}" ] && candidates+=("$XDG_CONFIG_HOME/.foundry/bin/cast")
    candidates+=("$HOME/.config/.foundry/bin/cast" "$HOME/.foundry/bin/cast")
    local c
    for c in "${candidates[@]}"; do
        [ -x "$c" ] && { printf '%s' "$c"; return 0; }
    done
    return 1
}

echo
CAST="$(find_cast || true)"
if [ -z "$CAST" ]; then
    echo "cast not found -- skipping wallet setup (run scripts/install-foundry.sh to install it)."
    WALLET_ADDR=""
else
    mkdir -p "$WALLET_DIR"
    if [ ! -f "$WALLET_PASSWORD_FILE" ]; then
        openssl rand -hex 16 >"$WALLET_PASSWORD_FILE"
        chmod 600 "$WALLET_PASSWORD_FILE"
    fi
    if [ ! -f "$WALLET_KEYSTORE" ]; then
        "$CAST" wallet new "$WALLET_DIR" "$WALLET_NAME" \
            --unsafe-password "$(cat "$WALLET_PASSWORD_FILE")" >/dev/null
        echo "Created cast wallet '$WALLET_NAME' at $WALLET_KEYSTORE"
    fi
    WALLET_ADDR="$("$CAST" wallet address --keystore "$WALLET_KEYSTORE" --password-file "$WALLET_PASSWORD_FILE")"
    echo "cast wallet '$WALLET_NAME': $WALLET_ADDR"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

echo
echo "================================================================"
echo " Regtest stack is up. All auth is via bitcoind's RPC cookie file"
echo " ($COOKIE_FILE) -- no rpcuser/rpcpassword anywhere."
echo
echo "   bitcoind          rpc   127.0.0.1:$BITCOIND_RPC_PORT   (datadir: $BITCOIN_DATADIR)"
echo "   bip300301_enforcer rpc  $ENFORCER_RPC_ADDR"
echo "                      grpc $ENFORCER_GRPC_ADDR   <- beth connects here"
echo "   electrs           electrum 127.0.0.1:$ELECTRS_RPC_PORT   http 127.0.0.1:$ELECTRS_HTTP_PORT"
echo "   beth              http  127.0.0.1:$BETH_HTTP_PORT        ws   127.0.0.1:$BETH_WS_PORT"
echo
if [ -n "$WALLET_ADDR" ]; then
    echo "   cast wallet '$WALLET_NAME': $WALLET_ADDR"
    echo "     keystore: $WALLET_KEYSTORE  (password: $WALLET_PASSWORD_FILE)"
    echo "     e.g. cast balance --rpc-url http://127.0.0.1:$BETH_HTTP_PORT $WALLET_ADDR"
    echo "          cast send --keystore $WALLET_KEYSTORE --password-file $WALLET_PASSWORD_FILE \\"
    echo "            --rpc-url http://127.0.0.1:$BETH_HTTP_PORT <to> --value <wei>"
    echo
fi
echo " Logs: $LOGDIR/{bitcoind,electrs,enforcer,beth}.log"
echo
echo " Note: this only starts and connects the four processes. It does not propose/ack"
echo " the beth sidechain on L1 or drive BIP301 blind-merge-mining -- that's a separate"
echo " step against the enforcer's WalletService (CreateSidechainProposal / AckSidechain"
echo " / the mining RPCs at $ENFORCER_RPC_ADDR)."
echo
echo " Press Ctrl-C to stop everything."
echo "================================================================"

wait -n "${PIDS[@]}"
echo "One of the processes exited; tearing down the rest."
