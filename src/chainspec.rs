//! Chain spec parser that defaults `--chain` to beth's own genesis instead of Ethereum
//! mainnet, so the node runs as this BIP300/301 sidechain out of the box.

use std::sync::Arc;

use reth_chainspec::ChainSpec;
use reth_cli::chainspec::{ChainSpecParser, parse_genesis};
use reth_ethereum_cli::chainspec::chain_value_parser;

/// beth's genesis, embedded at compile time. Predeploys [`WithdrawalRequestQueue`] (see
/// `contracts/WithdrawalRequestQueue.sol`) at `0x000000000000000000000000000000000000B300`, and
/// activates every hardfork through Osaka from timestamp 0.
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
