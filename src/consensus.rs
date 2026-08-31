//! Consensus wrapper that additionally requires each block to be committed in a Bitcoin
//! mainchain block per BIP301 blind merge mining, and that its BIP300 deposit-mint withdrawals
//! match what any node can independently recompute from the enforcer, on top of the standard
//! Ethereum checks.

use std::{fmt::Debug, sync::Arc};

use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::B256;
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_consensus::{Consensus, ConsensusError, FullConsensus, HeaderValidator, ReceiptRootBloom};
use reth_ethereum_consensus::EthBeaconConsensus;
use reth_ethereum_primitives::EthPrimitives;
use reth_execution_types::BlockExecutionResult;
use reth_node_builder::{
    BuilderContext,
    components::ConsensusBuilder,
    node::{FullNodeTypes, NodeTypes},
};
use reth_primitives_traits::{
    AlloyBlockHeader, Block, BlockBody, BlockHeader, NodePrimitives, RecoveredBlock, SealedBlock,
    SealedHeader,
};
use reth_storage_api::HeaderProvider;

use crate::enforcer::{self, EnforcerClient};

/// 1 satoshi = 10 gwei = 10^10 wei — matches [`crate::payload`]'s scaling. Kept as a separate
/// constant here (rather than shared) since the two are independent, deliberately-matching
/// pieces: the builder mints at this rate, and this is what consensus checks it minted at.
const GWEI_PER_SAT: u64 = 10;

/// Wraps [`EthBeaconConsensus`] and additionally requires:
/// - each block's hash be BIP301 blind-merge-mining committed in the Bitcoin mainchain block
///   referenced by its `extraData` field (see [`crate::payload`] for how that field is
///   populated), and
/// - the block's EIP-4895 withdrawals *begin* with exactly the BIP300 deposits any node can
///   independently recompute for the mainchain range `(parent's extraData, this block's
///   extraData]` from the enforcer — the same range, and same conversion, [`crate::payload`]
///   uses to mint them.
///
/// Only that deposit *prefix* is verified. `beth`'s payload builder can also append
/// withdrawal-bundle-refund credits after the deposits (see [`crate::withdrawal_bundle`]) —
/// those aren't independently verifiable yet, since no durable, cross-node bundle-status
/// tracking exists (see that module's doc comment), so a block producer could still forge
/// refunds today. Extending this check to cover them is the natural next step once that
/// tracking exists.
#[derive(Debug, Clone)]
pub struct Bip300301Consensus<ChainSpec, Provider> {
    inner: EthBeaconConsensus<ChainSpec>,
    enforcer: EnforcerClient,
    sidechain_id: u32,
    /// Used to look up parent headers by hash during post-execution validation (which, unlike
    /// `validate_header_against_parent`, isn't handed the parent directly). Sees in-memory
    /// (not-yet-persisted) blocks too, not just the database — reth's standard
    /// `BlockchainProvider` merges both.
    provider: Provider,
}

impl<ChainSpec: EthChainSpec + EthereumHardforks, Provider>
    Bip300301Consensus<ChainSpec, Provider>
{
    pub fn new(
        chain_spec: Arc<ChainSpec>,
        enforcer: EnforcerClient,
        sidechain_id: u32,
        provider: Provider,
    ) -> Self {
        Self {
            inner: EthBeaconConsensus::new(chain_spec),
            enforcer,
            sidechain_id,
            provider,
        }
    }
}

impl<H, ChainSpec, Provider> HeaderValidator<H> for Bip300301Consensus<ChainSpec, Provider>
where
    H: BlockHeader,
    ChainSpec: EthChainSpec<Header = H> + EthereumHardforks + Debug + Send + Sync,
    Provider: Debug + Send + Sync,
{
    fn validate_header(&self, header: &SealedHeader<H>) -> Result<(), ConsensusError> {
        self.inner.validate_header(header)?;

        let extra_data = header.header().extra_data();
        let prev_main_hash = B256::try_from(extra_data.as_ref()).map_err(|_| {
            ConsensusError::msg(format!(
                "block {} (number {}) extraData must be exactly 32 bytes (the BIP301 mainchain \
                 block it was BMM-mined against), got {} bytes",
                header.hash(),
                header.header().number(),
                extra_data.len(),
            ))
        })?;

        let committed = self
            .enforcer
            .is_committed(header.hash(), prev_main_hash, self.sidechain_id)
            .map_err(ConsensusError::other)?;

        if !committed {
            return Err(ConsensusError::msg(format!(
                "block {} (number {}) is not BIP301 BMM-committed in mainchain block {prev_main_hash}",
                header.hash(),
                header.header().number(),
            )));
        }

        Ok(())
    }

    fn validate_header_against_parent(
        &self,
        header: &SealedHeader<H>,
        parent: &SealedHeader<H>,
    ) -> Result<(), ConsensusError> {
        self.inner.validate_header_against_parent(header, parent)
    }
}

impl<B, ChainSpec, Provider> Consensus<B> for Bip300301Consensus<ChainSpec, Provider>
where
    B: Block,
    ChainSpec: EthChainSpec<Header = B::Header> + EthereumHardforks + Debug + Send + Sync,
    Provider: Debug + Send + Sync,
{
    fn validate_body_against_header(
        &self,
        body: &B::Body,
        header: &SealedHeader<B::Header>,
    ) -> Result<(), ConsensusError> {
        <EthBeaconConsensus<ChainSpec> as Consensus<B>>::validate_body_against_header(
            &self.inner,
            body,
            header,
        )
    }

    fn validate_block_pre_execution(&self, block: &SealedBlock<B>) -> Result<(), ConsensusError> {
        self.inner.validate_block_pre_execution(block)
    }
}

impl<N, ChainSpec, Provider> FullConsensus<N> for Bip300301Consensus<ChainSpec, Provider>
where
    N: NodePrimitives,
    ChainSpec: Send + Sync + EthChainSpec<Header = N::BlockHeader> + EthereumHardforks + Debug,
    Provider: HeaderProvider<Header = N::BlockHeader> + Send + Sync + Debug,
{
    fn validate_block_post_execution(
        &self,
        block: &RecoveredBlock<N::Block>,
        result: &BlockExecutionResult<N::Receipt>,
        receipt_root_bloom: Option<ReceiptRootBloom>,
        block_access_list_hash: Option<B256>,
    ) -> Result<(), ConsensusError> {
        <EthBeaconConsensus<ChainSpec> as FullConsensus<N>>::validate_block_post_execution(
            &self.inner,
            block,
            result,
            receipt_root_bloom,
            block_access_list_hash,
        )?;

        // Verify the block's EIP-4895 withdrawals begin with exactly the BIP300 deposits any
        // node can independently recompute for this block's own committed mainchain range —
        // see the struct doc comment for what this does and doesn't cover.
        let parent_header = self
            .provider
            .header(block.parent_hash())
            .map_err(ConsensusError::other)?
            .ok_or_else(|| {
                ConsensusError::msg(format!("parent header {} not found", block.parent_hash()))
            })?;
        let parent_main_hash = B256::try_from(parent_header.extra_data().as_ref()).ok();
        let main_hash = B256::try_from(block.extra_data().as_ref()).map_err(|_| {
            ConsensusError::msg(format!(
                "block {} extraData must be exactly 32 bytes",
                block.hash()
            ))
        })?;

        let expected_deposits = self
            .enforcer
            .deposits(self.sidechain_id, parent_main_hash, main_hash)
            .map_err(ConsensusError::other)?;

        let actual_withdrawals: &[_] = block
            .body()
            .withdrawals()
            .map_or(&[][..], |withdrawals| withdrawals.as_slice());
        deposit_prefix_matches(&expected_deposits, actual_withdrawals).map_err(|reason| {
            ConsensusError::msg(format!(
                "block {} deposit mints don't match mainchain range \
                 {parent_main_hash:?}..={main_hash}: {reason}",
                block.hash(),
            ))
        })?;

        Ok(())
    }
}

/// Checks that `actual` (a block's EIP-4895 withdrawals) begins with exactly `expected`
/// (BIP300 deposits, each converted to the same `{index, validator_index: 0, address, amount}`
/// shape [`crate::payload`] mints them as), in order. Returns `Err` with a human-readable
/// reason on the first mismatch.
fn deposit_prefix_matches(
    expected: &[enforcer::Deposit],
    actual: &[Withdrawal],
) -> Result<(), String> {
    if actual.len() < expected.len() {
        return Err(format!(
            "block has {} withdrawal(s), expected at least {} for its BIP300 deposit mints",
            actual.len(),
            expected.len(),
        ));
    }
    for (i, (deposit, withdrawal)) in expected.iter().zip(actual).enumerate() {
        let expected_amount = deposit.value_sats.saturating_mul(GWEI_PER_SAT);
        if withdrawal.index != i as u64
            || withdrawal.validator_index != 0
            || withdrawal.address != deposit.address
            || withdrawal.amount != expected_amount
        {
            return Err(format!(
                "withdrawal #{i} does not match the expected BIP300 deposit mint"
            ));
        }
    }
    Ok(())
}

/// Builder that wires [`Bip300301Consensus`] in as the node's consensus component.
#[derive(Debug, Clone)]
pub struct Bip300301ConsensusBuilder {
    enforcer: EnforcerClient,
    sidechain_id: u32,
}

impl Bip300301ConsensusBuilder {
    pub fn new(enforcer: EnforcerClient, sidechain_id: u32) -> Self {
        Self {
            enforcer,
            sidechain_id,
        }
    }
}

impl<Node> ConsensusBuilder<Node> for Bip300301ConsensusBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypes<ChainSpec: EthChainSpec + EthereumHardforks, Primitives = EthPrimitives>,
    >,
    Node::Provider: HeaderProvider<Header = <<Node::Types as NodeTypes>::Primitives as reth_node_builder::NodePrimitives>::BlockHeader>
        + Clone
        + Send
        + Sync
        + Debug
        + 'static,
{
    type Consensus =
        Arc<Bip300301Consensus<<Node::Types as NodeTypes>::ChainSpec, Node::Provider>>;

    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        Ok(Arc::new(Bip300301Consensus::new(
            ctx.chain_spec(),
            self.enforcer,
            self.sidechain_id,
            ctx.provider().clone(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;

    use super::*;

    fn deposit(address: u8, value_sats: u64) -> enforcer::Deposit {
        enforcer::Deposit {
            address: Address::repeat_byte(address),
            value_sats,
        }
    }

    fn deposit_mint(index: u64, address: u8, value_sats: u64) -> Withdrawal {
        Withdrawal {
            index,
            validator_index: 0,
            address: Address::repeat_byte(address),
            amount: value_sats.saturating_mul(GWEI_PER_SAT),
        }
    }

    #[test]
    fn empty_expected_and_actual_matches() {
        assert!(deposit_prefix_matches(&[], &[]).is_ok());
    }

    #[test]
    fn no_deposits_but_a_withdrawal_present_still_matches() {
        // Withdrawal-bundle refunds (not yet verified — see the struct doc comment) may be the
        // only entries in the list.
        let actual = [deposit_mint(0, 1, 100)];
        assert!(deposit_prefix_matches(&[], &actual).is_ok());
    }

    #[test]
    fn exact_match_succeeds() {
        let expected = [deposit(1, 100), deposit(2, 200)];
        let actual = [deposit_mint(0, 1, 100), deposit_mint(1, 2, 200)];
        assert!(deposit_prefix_matches(&expected, &actual).is_ok());
    }

    #[test]
    fn deposits_followed_by_extra_withdrawals_still_matches() {
        // The prefix is what's checked — anything after (e.g. future bundle refunds) is fine.
        let expected = [deposit(1, 100)];
        let actual = [deposit_mint(0, 1, 100), deposit_mint(1, 9, 999)];
        assert!(deposit_prefix_matches(&expected, &actual).is_ok());
    }

    #[test]
    fn fewer_withdrawals_than_expected_deposits_fails() {
        let expected = [deposit(1, 100), deposit(2, 200)];
        let actual = [deposit_mint(0, 1, 100)];
        assert!(deposit_prefix_matches(&expected, &actual).is_err());
    }

    #[test]
    fn wrong_amount_fails() {
        let expected = [deposit(1, 100)];
        let actual = [deposit_mint(0, 1, 999)];
        assert!(deposit_prefix_matches(&expected, &actual).is_err());
    }

    #[test]
    fn wrong_address_fails() {
        let expected = [deposit(1, 100)];
        let actual = [deposit_mint(0, 2, 100)];
        assert!(deposit_prefix_matches(&expected, &actual).is_err());
    }

    #[test]
    fn wrong_order_fails() {
        let expected = [deposit(1, 100), deposit(2, 200)];
        // Swapped relative to `expected`.
        let actual = [deposit_mint(0, 2, 200), deposit_mint(1, 1, 100)];
        assert!(deposit_prefix_matches(&expected, &actual).is_err());
    }

    #[test]
    fn wrong_index_fails() {
        let expected = [deposit(1, 100)];
        let mut actual = deposit_mint(0, 1, 100);
        actual.index = 5;
        assert!(deposit_prefix_matches(&expected, &[actual]).is_err());
    }

    #[test]
    fn nonzero_validator_index_fails() {
        // `validator_index` is always 0 for deposit mints (see payload.rs) — a nonzero value
        // isn't a deposit mint this function should accept.
        let expected = [deposit(1, 100)];
        let mut actual = deposit_mint(0, 1, 100);
        actual.validator_index = 1;
        assert!(deposit_prefix_matches(&expected, &[actual]).is_err());
    }
}
