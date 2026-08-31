//! Consensus wrapper that additionally requires each block to be committed in a Bitcoin
//! mainchain block per BIP301 blind merge mining, on top of the standard Ethereum checks.

use std::{fmt::Debug, sync::Arc};

use alloy_primitives::B256;
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_consensus::{
    Consensus, ConsensusError, FullConsensus, HeaderValidator, ReceiptRootBloom,
};
use reth_ethereum_consensus::EthBeaconConsensus;
use reth_ethereum_primitives::EthPrimitives;
use reth_execution_types::BlockExecutionResult;
use reth_node_builder::{
    components::ConsensusBuilder,
    node::{FullNodeTypes, NodeTypes},
    BuilderContext,
};
use reth_primitives_traits::{Block, BlockHeader, NodePrimitives, RecoveredBlock, SealedBlock, SealedHeader};

use crate::enforcer::EnforcerClient;

/// Wraps [`EthBeaconConsensus`] and additionally requires that each block's hash be
/// BIP301 blind-merge-mining committed in the Bitcoin mainchain block referenced by its
/// `extraData` field (see [`crate::payload`] for how that field is populated).
#[derive(Debug, Clone)]
pub struct Bip301Consensus<ChainSpec> {
    inner: EthBeaconConsensus<ChainSpec>,
    enforcer: EnforcerClient,
    sidechain_id: u32,
}

impl<ChainSpec: EthChainSpec + EthereumHardforks> Bip301Consensus<ChainSpec> {
    pub fn new(chain_spec: Arc<ChainSpec>, enforcer: EnforcerClient, sidechain_id: u32) -> Self {
        Self { inner: EthBeaconConsensus::new(chain_spec), enforcer, sidechain_id }
    }
}

impl<H, ChainSpec> HeaderValidator<H> for Bip301Consensus<ChainSpec>
where
    H: BlockHeader,
    ChainSpec: EthChainSpec<Header = H> + EthereumHardforks + Debug + Send + Sync,
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

impl<B, ChainSpec> Consensus<B> for Bip301Consensus<ChainSpec>
where
    B: Block,
    ChainSpec: EthChainSpec<Header = B::Header> + EthereumHardforks + Debug + Send + Sync,
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

impl<N, ChainSpec> FullConsensus<N> for Bip301Consensus<ChainSpec>
where
    N: NodePrimitives,
    ChainSpec: Send + Sync + EthChainSpec<Header = N::BlockHeader> + EthereumHardforks + Debug,
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
        )
    }
}

/// Builder that wires [`Bip301Consensus`] in as the node's consensus component.
#[derive(Debug, Clone)]
pub struct Bip301ConsensusBuilder {
    enforcer: EnforcerClient,
    sidechain_id: u32,
}

impl Bip301ConsensusBuilder {
    pub fn new(enforcer: EnforcerClient, sidechain_id: u32) -> Self {
        Self { enforcer, sidechain_id }
    }
}

impl<Node> ConsensusBuilder<Node> for Bip301ConsensusBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypes<ChainSpec: EthChainSpec + EthereumHardforks, Primitives = EthPrimitives>,
    >,
{
    type Consensus = Arc<Bip301Consensus<<Node::Types as NodeTypes>::ChainSpec>>;

    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        Ok(Arc::new(Bip301Consensus::new(ctx.chain_spec(), self.enforcer, self.sidechain_id)))
    }
}
