//! Payload builder that stamps each built block's `extraData` with the current BIP301
//! mainchain tip, so that [`crate::consensus::Bip300301Consensus`] has something to verify a BMM
//! commitment against once the block is BMM-mined into a Bitcoin block.
//!
//! BIP300 deposit minting and withdrawal-bundle lifecycle updates (both formerly handled here)
//! are now driven independently by [`crate::evm`]'s system calls, applied during block
//! execution itself — see [`crate::deposit_vault`]/[`crate::withdrawal_bundle`]'s module doc
//! comments. This builder no longer touches EIP-4895 withdrawals at all; the field is still set
//! (to an empty list) purely because post-Shanghai blocks require it to be present.
//!
//! This builder does still read [`crate::withdrawal_bundle`]'s queue on every block and, if a
//! bundle fits, broadcast it to the enforcer — purely as a side effect so the enforcer learns
//! about the bundle (block building/validation cannot make network calls). The bundle's actual
//! on-chain lifecycle (assignment, and refunding failed requests from the contract's own locked
//! balance) is driven independently by the system call above, from the exact same
//! [`withdrawal_bundle::select_withdrawal_bundle`] selection over the same parent state — so this
//! broadcast is redundant-but-harmless with what the system call is about to commit on-chain, not
//! a source of truth for it. See [`crate::withdrawal_bundle`]'s module doc comment.

use std::sync::Arc;

use alloy_primitives::{B256, Bytes};
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, MissingPayloadBehaviour, PayloadBuilder, PayloadConfig,
};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_ethereum_engine_primitives::{EthBuiltPayload, EthPayloadAttributes};
use reth_ethereum_payload_builder::{EthereumBuilderConfig, default_ethereum_payload};
use reth_ethereum_primitives::{EthPrimitives, TransactionSigned};
use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
use reth_evm_ethereum::EthEvmConfig;
use reth_node_builder::{
    BuilderContext, FullNodeTypes, NodeTypes, PayloadBuilderConfig, PayloadTypes, PrimitivesTy,
    TxTy, components::PayloadBuilderBuilder,
};
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_primitives_traits::AlloyBlockHeader;
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, PoolTransaction, TransactionPool,
    ValidPoolTransaction,
};

use crate::{enforcer::EnforcerClient, withdrawal_bundle};

type BestTransactionsIter<Pool> = Box<
    dyn BestTransactions<Item = Arc<ValidPoolTransaction<<Pool as TransactionPool>::Transaction>>>,
>;

/// Payload builder that delegates to [`default_ethereum_payload`], overriding `extraData` on
/// every build with the enforcer's current mainchain tip and broadcasting a withdrawal bundle
/// when one fits — see the module doc comment.
#[derive(Debug, Clone)]
pub struct Bip300301PayloadBuilder<Pool, Client, EvmConfig = EthEvmConfig> {
    client: Client,
    pool: Pool,
    evm_config: EvmConfig,
    builder_config: EthereumBuilderConfig,
    enforcer: EnforcerClient,
    sidechain_id: u32,
}

impl<Pool, Client, EvmConfig> Bip300301PayloadBuilder<Pool, Client, EvmConfig> {
    pub fn new(
        client: Client,
        pool: Pool,
        evm_config: EvmConfig,
        builder_config: EthereumBuilderConfig,
        enforcer: EnforcerClient,
        sidechain_id: u32,
    ) -> Self {
        Self {
            client,
            pool,
            evm_config,
            builder_config,
            enforcer,
            sidechain_id,
        }
    }

    /// Fetches the current mainchain tip, returning the builder config with `extraData` set to
    /// it, so that [`crate::consensus::Bip300301Consensus`] has something to verify a BMM
    /// commitment against once the block is BMM-mined into a Bitcoin block.
    ///
    /// Also selects a BIP300 withdrawal bundle from `parent_hash`'s state (if one fits) and
    /// broadcasts it to the enforcer — see the module doc comment for why this is a
    /// redundant-but-harmless side effect, not the source of truth for the bundle's on-chain
    /// lifecycle.
    fn next_block_context(
        &self,
        parent_hash: B256,
        parent_number: u64,
    ) -> Result<EthereumBuilderConfig, PayloadBuilderError>
    where
        Client: StateProviderFactory,
    {
        let main_tip = self
            .enforcer
            .chain_tip()
            .map_err(PayloadBuilderError::other)?;

        let builder_config = self
            .builder_config
            .clone()
            .with_extra_data(Bytes::copy_from_slice(main_tip.as_slice()));

        // Select and broadcast a withdrawal bundle, if one fits. A failure here is deliberately
        // non-fatal: broadcasting is a best-effort side effect for the enforcer's benefit, not
        // something the on-chain lifecycle (driven by `crate::evm`'s system call) depends on.
        let next_block_number = u32::try_from(parent_number.saturating_add(1)).unwrap_or(u32::MAX);
        match self.client.state_by_block_hash(parent_hash) {
            Ok(state) => match withdrawal_bundle::read_pending_withdrawals(state.as_ref()) {
                Ok(pending) => {
                    if let Some(bundle) =
                        withdrawal_bundle::select_withdrawal_bundle(&pending, next_block_number)
                    {
                        match self
                            .enforcer
                            .broadcast_withdrawal_bundle(self.sidechain_id, &bundle.tx)
                        {
                            Ok(()) => {
                                tracing::info!(
                                    m6id = %bundle.m6id(),
                                    requests = ?bundle.request_indices,
                                    outputs = bundle.tx.output.len(),
                                    "broadcast BIP300 withdrawal bundle",
                                );
                            }
                            Err(err) => {
                                tracing::warn!(%err, m6id = %bundle.m6id(), "failed to broadcast withdrawal bundle");
                            }
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(%err, "failed to read WithdrawalRequestQueue");
                }
            },
            Err(err) => {
                tracing::warn!(%err, "failed to load parent state for withdrawal bundle processing");
            }
        }

        Ok(builder_config)
    }
}

impl<Pool, Client, EvmConfig> PayloadBuilder for Bip300301PayloadBuilder<Pool, Client, EvmConfig>
where
    EvmConfig: ConfigureEvm<Primitives = EthPrimitives, NextBlockEnvCtx = NextBlockEnvAttributes>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthereumHardforks> + Clone,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>,
{
    type Attributes = EthPayloadAttributes;
    type BuiltPayload = EthBuiltPayload;

    fn try_build(
        &self,
        mut args: BuildArguments<EthPayloadAttributes, EthBuiltPayload>,
    ) -> Result<BuildOutcome<EthBuiltPayload>, PayloadBuilderError> {
        let builder_config = self.next_block_context(
            args.config.parent_header.hash(),
            args.config.parent_header.header().number(),
        )?;
        // Post-Shanghai blocks require this field to be present — `beth` no longer uses it for
        // anything (see module doc comment), so it's always empty.
        args.config.attributes.withdrawals = Some(Vec::new());

        default_ethereum_payload(
            self.evm_config.clone(),
            self.client.clone(),
            self.pool.clone(),
            builder_config,
            args,
            |attributes: BestTransactionsAttributes| {
                self.pool.best_transactions_with_attributes(attributes)
            },
        )
    }

    fn on_missing_payload(
        &self,
        _args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> MissingPayloadBehaviour<Self::BuiltPayload> {
        if self.builder_config.await_payload_on_missing {
            MissingPayloadBehaviour::AwaitInProgress
        } else {
            MissingPayloadBehaviour::RaceEmptyPayload
        }
    }

    fn build_empty_payload(
        &self,
        mut config: PayloadConfig<Self::Attributes>,
    ) -> Result<EthBuiltPayload, PayloadBuilderError> {
        let builder_config = self.next_block_context(
            config.parent_header.hash(),
            config.parent_header.header().number(),
        )?;
        // Post-Shanghai blocks require this field to be present — `beth` no longer uses it for
        // anything (see module doc comment), so it's always empty.
        config.attributes.withdrawals = Some(Vec::new());

        let args = BuildArguments::new(
            Default::default(),
            Default::default(),
            None,
            config,
            Default::default(),
            None,
        );

        default_ethereum_payload(
            self.evm_config.clone(),
            self.client.clone(),
            self.pool.clone(),
            builder_config,
            args,
            |_| -> BestTransactionsIter<Pool> { Box::new(std::iter::empty()) },
        )?
        .into_payload()
        .ok_or(PayloadBuilderError::MissingPayload)
    }
}

/// Builder that wires [`Bip300301PayloadBuilder`] in as the node's payload builder component.
#[derive(Debug, Clone)]
pub struct Bip300301PayloadBuilderBuilder {
    enforcer: EnforcerClient,
    sidechain_id: u32,
}

impl Bip300301PayloadBuilderBuilder {
    pub fn new(enforcer: EnforcerClient, sidechain_id: u32) -> Self {
        Self {
            enforcer,
            sidechain_id,
        }
    }
}

impl<Types, Node, Pool, Evm> PayloadBuilderBuilder<Node, Pool, Evm>
    for Bip300301PayloadBuilderBuilder
where
    Types: NodeTypes<ChainSpec: EthereumHardforks, Primitives = EthPrimitives>,
    Node: FullNodeTypes<Types = Types>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TxTy<Node::Types>>>
        + Unpin
        + 'static,
    Evm: ConfigureEvm<Primitives = PrimitivesTy<Types>, NextBlockEnvCtx = NextBlockEnvAttributes>
        + 'static,
    Types::Payload:
        PayloadTypes<BuiltPayload = EthBuiltPayload, PayloadAttributes = EthPayloadAttributes>,
{
    type PayloadBuilder = Bip300301PayloadBuilder<Pool, Node::Provider, Evm>;

    async fn build_payload_builder(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm_config: Evm,
    ) -> eyre::Result<Self::PayloadBuilder> {
        let conf = ctx.payload_builder_config();
        let chain = ctx.chain_spec().chain();
        let gas_limit = conf.gas_limit_for(chain);
        let skip_state_root = ctx.config().tree_config().skip_state_root();

        Ok(Bip300301PayloadBuilder::new(
            ctx.provider().clone(),
            pool,
            evm_config,
            EthereumBuilderConfig::new()
                .with_gas_limit(gas_limit)
                .with_max_blobs_per_block(conf.max_blobs_per_block())
                .with_skip_state_root(skip_state_root),
            self.enforcer,
            self.sidechain_id,
        ))
    }
}
