# beth

`beth` is an EVM-compatible Bitcoin sidechain, secured by the
[BIP300](https://github.com/bitcoin/bips/blob/master/bip-0300.mediawiki)/[BIP301](https://github.com/bitcoin/bips/blob/master/bip-0301.mediawiki)
"drivechain" two-way peg. It's a [reth](https://github.com/paradigmxyz/reth) node -- execution
layer only, no consensus or wallet of its own beyond what's described below -- with a custom
consensus module, payload builder, and EVM wiring that implement the sidechain side of the peg:
Bitcoin deposits credited as ETH, ETH withdrawals paid out as Bitcoin, and each sidechain block
committing to a Bitcoin mainchain block via blind merged mining (BMM).

`beth` doesn't implement the mainchain side of BIP300/301 itself (sidechain proposals/acks,
withdrawal-bundle voting, wallet management) -- that's the job of a separate, chain-agnostic
[`bip300301_enforcer`](https://github.com/LayerTwo-Labs/bip300301_enforcer) node, which `beth`
talks to over gRPC for everything peg-related. `beth` cannot run meaningfully on its own; it
needs a running enforcer (and, behind that, a regtest/real Bitcoin node) to talk to.

## How the peg works

- **Deposits** (Bitcoin -> `beth`): a user sends BTC to the sidechain's mainchain treasury
  output. The enforcer recognizes it and reports it to `beth`. On its next block, every node
  independently recomputes the same `DepositVault.systemCreditDeposits(...)` call from that
  already-agreed mainchain data -- a "system call" (the same no-transaction,
  protocol-level-state-write mechanism EIP-4788/2935/7002 use), gated on `msg.sender ==
  SYSTEM_ADDRESS`, applied during ordinary block execution. A forged or missing credit is
  therefore caught by standard state-root validation, with no bespoke consensus check needed.
- **Withdrawals** (`beth` -> Bitcoin): a user calls `WithdrawalRequestQueue.requestWithdrawal()`.
  `beth`'s own payload builder deterministically selects pending requests into a bundle on every
  block it builds and broadcasts the resulting BIP300 "M6" Bitcoin transaction to the enforcer.
  Mainchain miners vote the bundle through; once it's confirmed, the enforcer pays it out on L1,
  and the same system-call mechanism as deposits (`WithdrawalRequestQueue.systemReportBundleLifecycle`)
  advances each request's on-chain status.
- **BMM**: every `beth` block's `extraData` commits to the current Bitcoin mainchain tip. A
  "critical data transaction" bid for that block's hash gets embedded in the *next* mainchain
  block's coinbase (the BIP301 M7 accept message) -- `beth`'s consensus module checks the mainchain
  tip and a small window of recent ancestors for that commitment before accepting the block.

## Layout

- [`contracts/DepositVault.sol`](contracts/DepositVault.sol) -- holds the sidechain's ETH
  reserve (genesis-funded); credits deposits via the system call above.
- [`contracts/WithdrawalRequestQueue.sol`](contracts/WithdrawalRequestQueue.sol) -- queues
  withdrawal requests and tracks their lifecycle (`Pending` -> `Bundled` -> `Confirmed`/
  `Refunded`); self-funding escrow, not genesis-funded.
- `src/main.rs` -- wires the pieces below into a standard reth node (`EthereumNode`), pointed at
  an enforcer via `BIP300301_ENFORCER_URL` (default `http://127.0.0.1:8080`) and a sidechain slot
  via `BETH_SIDECHAIN_ID` (default `10`).
- `src/consensus.rs` -- wraps reth's standard Ethereum consensus with the BIP301 BMM commitment
  check described above.
- `src/payload.rs` -- wraps reth's standard payload builder to stamp `extraData` with the current
  mainchain tip and to select/broadcast withdrawal bundles.
- `src/evm.rs` -- wraps reth's standard EVM config to run the deposit-crediting and
  withdrawal-lifecycle system calls once per block, for both block building and block validation.
- `src/deposit_vault.rs` / `src/withdrawal_bundle.rs` -- the Rust-side logic for each system call
  (reading pending withdrawals and selecting a bundle; building the deposit-credit calldata).
- `src/enforcer.rs` / `src/proto.rs` -- the gRPC client for the enforcer's `ValidatorService`,
  generated from its own `.proto` definitions (see `build.rs`) -- the same approach
  [`thunder-rust`](https://github.com/LayerTwo-Labs/thunder-rust), a reference UTXO-based
  BIP300/301 sidechain, uses.
- `src/chainspec.rs` -- `beth`'s own genesis (`genesis.json`, chain ID `300301`), embedded at
  compile time and used by default. Currently activates hardforks through Prague only,
  deliberately -- see the doc comment there for why (a real, confirmed limitation in this pinned
  reth version's Osaka/Amsterdam support, not something left undone).

## Building

```bash
cargo build --release
```

## Running it against a local regtest devnet

`beth` needs a `bip300301_enforcer` (and, behind that, a patched, BIP300/301-aware Bitcoin node)
to talk to before it does anything useful. **See [`scripts/`](scripts/) and in particular
[`scripts/README.md`](scripts/README.md)** for everything needed to build that companion
infrastructure, bring up a full local regtest devnet, and drive it end to end -- block
production, deposits, withdrawals, and testing `beth` against a real, unmodified Foundry
workflow.
