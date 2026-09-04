//! Chain spec parser that defaults `--chain` to beth's own genesis instead of Ethereum
//! mainnet, so the node runs as this BIP300/301 sidechain out of the box.

use std::sync::Arc;

use reth_chainspec::ChainSpec;
use reth_cli::chainspec::{ChainSpecParser, parse_genesis};
use reth_ethereum_cli::chainspec::chain_value_parser;

/// beth's genesis, embedded at compile time. Predeploys [`WithdrawalRequestQueue`] (see
/// `contracts/WithdrawalRequestQueue.sol`) at `0x000000000000000000000000000000000000B300`, and
/// activates every hardfork through Prague from timestamp 0.
///
/// Deliberately stops at Prague rather than also activating Osaka: this reth build's Osaka
/// support (`engine_newPayloadV5`) requires a `blockAccessList` on every submitted payload, but
/// validates it against the separate, experimental "Amsterdam" fork -- which this chain doesn't
/// (and, being unreleased/unstable, shouldn't) activate -- rejecting any value for that field
/// unconditionally. That combination makes Osaka activation unusable for payload submission on
/// this reth version; Prague is already sufficient for standard contract testing.
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
