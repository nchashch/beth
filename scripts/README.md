# BIP300/301 regtest devnet

Scripts to build and drive a local drivechain devnet for `beth` (this repo's reth-based
BIP300/301 sidechain): a regtest `bitcoind`, `bip300301_enforcer`, `electrs`, and `beth` all
wired together, plus test scripts that exercise the full L1<->L2 peg (deposits, withdrawals) and
beth's EVM compatibility end to end.

## 1. Workspace layout

These scripts expect to live two directories below a common workspace root, alongside sibling
checkouts of every other component:

```
<workspace root>/
├── bitcoin-patched/        # LayerTwo-Labs' drivechain fork of Bitcoin Core
├── bip300301_enforcer/     # BIP300/301 enforcer node
├── electrs/                # Electrum server (indexes bitcoind for the enforcer's wallet)
├── beth/                   # this repo
│   └── scripts/            # <- you are here
├── DATADIR/                # generated: all per-process data/logs live here
└── contracts-test/         # generated: scaffolded by test-contracts.sh
```

`ROOT` (the workspace root) is computed by each script as two directories up from itself, so if
you move `scripts/` anywhere else relative to the other checkouts, update that computation
(`ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"` near the top of each script)
accordingly, or export the individual `*_CLI`/`*_DATADIR` overrides each script also accepts
(see each script's own header comment).

`DATADIR/` and `contracts-test/` are created for you the first time you run the scripts below --
nothing needs to exist ahead of time except the four repo checkouts.

## 2. Prerequisites

- A Rust toolchain (for `bip300301_enforcer`, `electrs`, and `beth`).
- CMake and the usual Bitcoin Core build dependencies (for `bitcoin-patched`) -- including
  `libzmq3-dev` (or your distro's equivalent) specifically, since ZMQ support is off by default
  and this devnet needs it (see the build command below).
- `curl`, `jq`, `openssl` on `PATH`.
- [Foundry](https://getfoundry.sh) (`cast`/`forge`) -- install with:
  ```
  scripts/install-foundry.sh
  ```
  Safe to re-run; installs `foundryup` if missing, then always runs it to install/update
  `forge`/`cast`/`anvil`/`chisel`.

## 3. Clone and build the sibling repos

```bash
scripts/setup-workspace.sh
```

Clones `bitcoin-patched`, `bip300301_enforcer`, and `electrs` into the workspace root (skipping
any that are already checked out, leaving them untouched), builds all three with the right
config (in particular `-DWITH_ZMQ=ON` for `bitcoin-patched` -- see below), and creates an empty
`DATADIR/`. Safe to re-run; every build is incremental via cargo's/cmake's own caching. Doesn't
build `beth` itself (you're already in it) or install Foundry -- do those two separately:

```bash
(cd beth && cargo build --release)
scripts/install-foundry.sh
```

Equivalent by hand, if you'd rather clone/build individually or need to customize a step (this is
exactly what `scripts/setup-workspace.sh` automates):

```bash
# From the workspace root:
git clone git@github.com:LayerTwo-Labs/bitcoin-patched.git
git clone git@github.com:LayerTwo-Labs/bip300301_enforcer.git
git clone https://github.com/mempool/electrs      # 'mempool' branch, used here

# bitcoin-patched -- CMake-based (branches off Bitcoin Core v29.2 + drivechain patches).
# -DWITH_ZMQ=ON is required, not just nice-to-have: it defaults OFF upstream, but
# regtest-up.sh's bitcoin.conf enables zmqpubhashblock/rawblock/hashtx/rawtx/sequence, which
# bip300301_enforcer depends on for block/mempool notifications -- without it, bitcoind starts
# fine but the enforcer never sees new blocks.
cmake -B bitcoin-patched/build -S bitcoin-patched -DWITH_ZMQ=ON
cmake --build bitcoin-patched/build -j"$(nproc)"

# bip300301_enforcer
(cd bip300301_enforcer && cargo build --release)

# electrs (mempool/electrs fork, "mempool" branch -- already the default on clone;
# --bin electrs since the crate builds more than one binary)
(cd electrs && cargo build --release --bin electrs)
```

A debug build (plain `cargo build`, no `--release`) works too -- `regtest-up.sh` prefers a
release binary if present and falls back to debug automatically for `bip300301_enforcer`,
`electrs`, and `beth`. `bitcoin-patched` only has the one `build/` output either way.

## 4. Bring the stack up

```bash
scripts/regtest-up.sh
```

Starts, in order, `bitcoind` (regtest, cookie auth -- no `rpcuser`/`rpcpassword` anywhere; mines
101 blocks on first run so there's a spendable coinbase), `electrs`, `bip300301_enforcer`
(wallet auto-created, synced via `electrs`), and `beth` (wired to the enforcer's gRPC). It also
creates a `cast` wallet at `$DATADIR/wallet` for signing transactions against beth, if `cast` is
on `PATH` or found via `scripts/install-foundry.sh`'s install location.

Runs in the foreground; `Ctrl-C` (or any one of the four processes dying) tears the rest down.
Logs land at `$DATADIR/logs/{bitcoind,electrs,enforcer,beth}.log`. All the ports, and every
`*_DATADIR`/`*_PORT` default, are overridable via environment variables -- see the top of the
script.

This only starts and connects the four processes -- it does **not** activate the sidechain or
drive block production. That's the next two steps, each against the already-running stack, so
run them from a second terminal (or background `regtest-up.sh` itself).

## 5. Activate the sidechain

```bash
scripts/activate-sidechain.sh 10      # 10 is beth's slot in this repo's setup; also the default
```

Submits a BIP300 sidechain proposal (M1) and mines enough ACK blocks (M2) to activate it.
Idempotent -- safe to re-run; does nothing if the slot is already active.

## 6. Drive BIP301 blind-merge-mining (BMM)

```bash
scripts/bmm-mine.sh 10
```

**This needs to run continuously, in its own terminal (or backgrounded), for anything else here
to work.** Nothing else in this devnet drives beth's block production -- without this loop
running, beth never imports a new block, so no transaction (a transfer, a deposit credit, a
withdrawal bundle, a deployed contract, anything) ever confirms. It bids for and mines one BMM
commitment roughly once a second; see the script's own header comment for exactly what each
iteration does. `Ctrl-C` to stop; safe to re-run afterwards (or after a crash) -- it tracks its
own last-mined head under `$DATADIR/beth/.bmm-head` and resumes from there rather than
restarting from genesis.

Expect a burst of `BMM bid rejected: Insufficient funds` lines for roughly the first minute after
a fresh start (the enforcer's wallet syncs via periodic polling, not instantly) -- this self-heals
once it catches up, not a real stall. See "Known issues" below for a different failure mode that
does *not* self-heal.

## 7. Run the test scripts

With `bmm-mine.sh` running in the background, each of these can be run on its own (against
sidechain 10, or pass a different `SIDECHAIN_ID` if you activated a different slot):

```bash
# Deposit BTC to the cast wallet's address, confirm it, and check it lands on beth.
scripts/test-deposit.sh 10 [AMOUNT_SATS] [FEE_SATS] [RECIPIENT]
#   AMOUNT_SATS defaults to 100000000 (1 BTC); RECIPIENT defaults to the cast wallet's address.

# Send a real requestWithdrawal() tx, let beth auto-bundle/broadcast it, vote it through
# mainchain mining, and verify the BTC destination actually receives the payout.
scripts/test-withdrawal.sh 10 [PAYOUT_SATS] [FEE_SATS] [DEST_ADDRESS]
#   PAYOUT_SATS defaults to 10000000 (0.1 BTC); DEST_ADDRESS defaults to a fresh address
#   from bitcoind's own "regtest" wallet.

# Deposit funds, scaffold Foundry's own unmodified project template (forge init), deploy +
# interact with it via a real forge script --broadcast, and run its own `forge test --fork-url`
# suite against a live fork of beth's state.
scripts/test-contracts.sh 10
```

Each is independently runnable and re-runnable; none of them tear the stack down. All three need
`bitcoin-patched` specifically, not stock Bitcoin Core -- see "Known issues" below.

## 8. Resetting

```bash
scripts/regtest-reset.sh          # refuses to run while the stack is up
scripts/regtest-reset.sh --force  # stops it for you first
scripts/regtest-reset.sh --yes    # skip the confirmation prompt (e.g. for scripting)
```

Wipes bitcoind's chain/wallets, the enforcer's wallet/validator state, electrs' index, and
beth's chain db -- back to state 0. Keeps `$DATADIR/bitcoin/bitcoin.conf` by default (so custom
ports/zmq addresses survive a reset); pass `--purge-config` to remove it too. Run
`scripts/regtest-up.sh` again afterwards to start fresh.

A typical full cycle:

```bash
scripts/regtest-up.sh &                 # or run in its own terminal, foreground
scripts/activate-sidechain.sh 10
scripts/bmm-mine.sh 10 &                # or its own terminal; must keep running
scripts/test-deposit.sh 10
scripts/test-withdrawal.sh 10
scripts/test-contracts.sh 10
```

## Known issues

- **`bitcoin-patched`, not stock Bitcoin Core.** Deposits past the first, and withdrawals, spend
  a sidechain's `OP_DRIVECHAIN`-tagged treasury output -- this needs actual BIP300 consensus
  rules that only `bitcoin-patched` implements. Against stock/unpatched Bitcoin Core, every
  deposit past the first fails with `mempool-script-verify-flag-failed (NOPx reserved for
  soft-fork upgrades)`.
- **`beth`'s genesis stops at Prague, deliberately.** Both Osaka ("Fusaka", mainnet's current
  fork) and Amsterdam ("Glamsterdam") were tried and reverted during development -- see the doc
  comment on `BETH_GENESIS_JSON` in `beth/src/chainspec.rs` for exactly why each is currently a
  dead end on the pinned reth version this repo builds against, not just something left undone.
- **The enforcer can stall under sustained load.** After enough sustained activity (BMM mining
  plus real transactions/deposits/withdrawals running for a while), `bip300301_enforcer` can get
  into a state where a stale, superseded BMM bid transaction lingers in bitcoind's mempool, and
  its own block producer keeps building block templates that include it -- which its own
  validator then rejects, every time, forever. Symptom: `bmm-mine.sh` iterations start failing
  with `deadline_exceeded: ... the enforcer has fallen behind Bitcoin Core, or rejected the
  block`, and the mainchain tip stops advancing even though `bmm-mine.sh` is still running. This
  is a bug in `bip300301_enforcer` itself (see `lib/block_producer/mine.rs` and
  `lib/validator/cusf_enforcer.rs`), not in these scripts or in `beth` -- the only known fix
  right now is `scripts/regtest-reset.sh` and starting over.
