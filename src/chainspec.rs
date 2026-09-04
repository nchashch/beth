//! Chain spec parser that defaults `--chain` to beth's own genesis instead of Ethereum
//! mainnet, so the node runs as this BIP300/301 sidechain out of the box.

use std::sync::Arc;

use reth_chainspec::ChainSpec;
use reth_cli::chainspec::{ChainSpecParser, parse_genesis};
use reth_ethereum_cli::chainspec::chain_value_parser;

/// beth's genesis, embedded at compile time. Predeploys [`WithdrawalRequestQueue`] (see
/// `contracts/WithdrawalRequestQueue.sol`) at `0x000000000000000000000000000000000000B300`, and
/// activates every hardfork through Prague from timestamp 0 -- one fork behind real mainnet
/// (currently on Osaka/"Fusaka"). Both Osaka and Amsterdam ("Glamsterdam") were tried and
/// reverted during development; each is currently a dead end on this pinned reth build (tag
/// `v2.5.0`), not just something left undone:
///
/// - **Osaka alone doesn't work**: `engine_newPayloadV5`'s payload parameter is typed as
///   `alloy_rpc_types_engine::ExecutionPayloadV4` -- a struct alloy's own doc comment describes
///   as "defined in the Amsterdam fork" -- whose `block_access_list`/`slot_number` fields are
///   plain required fields, not `Option`: omitting them fails JSON deserialization ("missing
///   field `blockAccessList`"), while supplying any value for them, including an empty/default
///   one, is then rejected by a separate semantic check (`validate_block_access_list_presence` in
///   `reth-payload-primitives`) as "block access list pre-Amsterdam" unless the chain's
///   `amsterdamTime` is *also* active. There is no value that satisfies both checks pre-Amsterdam.
/// - **Activating Amsterdam too gets past that** (confirmed: BMM mining, a deposit, and a full
///   withdrawal round-trip all worked -- reth's own `default_ethereum_payload` already builds a
///   real block access list automatically once Amsterdam is active, via
///   `crates/ethereum/payload/src/lib.rs`'s `.with_bal_builder_if(is_amsterdam)`, so no beth code
///   changes were needed there), but uncovers a second, worse problem: `eth_estimateGas` and
///   beth's real block-execution path disagree about gas cost once BAL tracking is active. A
///   plain contract deployment via the standard Foundry estimate-then-send workflow
///   (`forge script --broadcast`) reverted out-of-gas at `eth_estimateGas`'s number (203856),
///   then succeeded fine given a manually padded limit (500000, only 302542 actually used) --
///   i.e. `eth_estimateGas` under-quotes real cost by tens of thousands of gas once BAL tracking
///   is active. That breaks the estimate-then-send pattern essentially all Ethereum tooling
///   relies on by default, not just this repo's own scripts.
///
/// Both gaps track with reth's own Amsterdam/Osaka support being genuinely unfinished upstream
/// (the "Tracking: Osaka Hardfork" issue is still mostly open), so revisit this once a newer reth
/// release has matured past it -- Prague is sufficient for standard contract testing in the
/// meantime.
///
/// [`WithdrawalRequestQueue`]: https://github.com/LayerTwo-Labs/beth/blob/main/contracts/WithdrawalRequestQueue.sol
const BETH_GENESIS_JSON: &str = include_str!("../genesis.json");

/// Chain spec parser defaulting to beth's own genesis (`"beth"`, the first and therefore
/// default entry in `SUPPORTED_CHAINS`), while still accepting anything
/// [`chain_value_parser`] does (a builtin Ethereum chain name, or a path/raw JSON genesis).
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct BethChainSpecParser;

impl ChainSpecParser for BethChainSpecParser {
    type ChainSpec = ChainSpec;

    const SUPPORTED_CHAINS: &'static [&'static str] = &["beth"];

    fn parse(s: &str) -> eyre::Result<Arc<ChainSpec>> {
        if s == "beth" {
            return Ok(Arc::new(parse_genesis(BETH_GENESIS_JSON)?.into()));
        }
        chain_value_parser(s)
    }
}
