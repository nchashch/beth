//! Payload builder that stamps each built block's `extraData` with the current BIP301
//! mainchain tip, so that [`crate::consensus::Bip301Consensus`] has something to verify a BMM
//! commitment against once the block is BMM-mined into a Bitcoin block.
//!
//! This is also the natural place to eventually source L1 deposits (and settle L1 withdrawals)
//! from the enforcer when building a block, since it already runs once per payload job with
//! access to the enforcer and the block under construction.

use std::sync::Arc;

use alloy_primitives::Bytes;
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, MissingPayloadBehaviour, PayloadBuilder, PayloadConfig,
};
use reth_ethereum_engine_primitives::{EthBuiltPayload, EthPayloadAttributes};
use reth_ethereum_payload_builder::{default_ethereum_payload, EthereumBuilderConfig};
use reth_ethereum_primitives::{EthPrimitives, TransactionSigned};
use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
use reth_evm_ethereum::EthEvmConfig;
use reth_node_builder::{
    components::PayloadBuilderBuilder, BuilderContext, FullNodeTypes, NodeTypes,
    PayloadBuilderConfig, PayloadTypes, PrimitivesTy, TxTy,
};
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, PoolTransaction, TransactionPool,
    ValidPoolTransaction,
};

use crate::enforcer::EnforcerClient;

type BestTransactionsIter<Pool> = Box<
    dyn BestTransactions<Item = Arc<ValidPoolTransaction<<Pool as TransactionPool>::Transaction>>>,
>;

/// Payload builder that delegates to [`default_ethereum_payload`], overriding `extraData` on
/// every build with the enforcer's current mainchain tip.
#[derive(Debug, Clone)]
pub struct Bip301PayloadBuilder<Pool, Client, EvmConfig = EthEvmConfig> {
    client: Client,
    pool: Pool,
    evm_config: EvmConfig,
    builder_config: EthereumBuilderConfig,
    enforcer: EnforcerClient,
}

impl<Pool, Client, EvmConfig> Bip301PayloadBuilder<Pool, Client, EvmConfig> {
    pub const fn new(
        client: Client,
        pool: Pool,
        evm_config: EvmConfig,
        builder_config: EthereumBuilderConfig,
        enforcer: EnforcerClient,
    ) -> Self {
        Self { client, pool, evm_config, builder_config, enforcer }
    }

    /// The static builder config, with `extraData` overridden to the current BIP301 mainchain
    /// tip as reported by the enforcer.
    fn builder_config_for_next_block(&self) -> Result<EthereumBuilderConfig, PayloadBuilderError> {
        let main_tip = self.enforcer.chain_tip().map_err(PayloadBuilderError::other)?;
        Ok(self.builder_config.clone().with_extra_data(Bytes::copy_from_slice(main_tip.as_slice())))
    }
}

impl<Pool, Client, EvmConfig> PayloadBuilder for Bip301PayloadBuilder<Pool, Client, EvmConfig>
where
    EvmConfig: ConfigureEvm<Primitives = EthPrimitives, NextBlockEnvCtx = NextBlockEnvAttributes>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthereumHardforks> + Clone,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>,
{
    type Attributes = EthPayloadAttributes;
    type BuiltPayload = EthBuiltPayload;

    fn try_build(
        &self,
        args: BuildArguments<EthPayloadAttributes, EthBuiltPayload>,
    ) -> Result<BuildOutcome<EthBuiltPayload>, PayloadBuilderError> {
        default_ethereum_payload(
            self.evm_config.clone(),
            self.client.clone(),
            self.pool.clone(),
            self.builder_config_for_next_block()?,
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
        config: PayloadConfig<Self::Attributes>,
    ) -> Result<EthBuiltPayload, PayloadBuilderError> {
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
            self.builder_config_for_next_block()?,
            args,
            |_| -> BestTransactionsIter<Pool> { Box::new(std::iter::empty()) },
        )?
        .into_payload()
        .ok_or(PayloadBuilderError::MissingPayload)
    }
}

/// Builder that wires [`Bip301PayloadBuilder`] in as the node's payload builder component.
#[derive(Debug, Clone)]
pub struct Bip301PayloadBuilderBuilder {
    enforcer: EnforcerClient,
}

impl Bip301PayloadBuilderBuilder {
    pub fn new(enforcer: EnforcerClient) -> Self {
        Self { enforcer }
    }
}

impl<Types, Node, Pool, Evm> PayloadBuilderBuilder<Node, Pool, Evm> for Bip301PayloadBuilderBuilder
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
    type PayloadBuilder = Bip301PayloadBuilder<Pool, Node::Provider, Evm>;

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

        Ok(Bip301PayloadBuilder::new(
            ctx.provider().clone(),
            pool,
            evm_config,
            EthereumBuilderConfig::new()
                .with_gas_limit(gas_limit)
                .with_max_blobs_per_block(conf.max_blobs_per_block())
                .with_skip_state_root(skip_state_root),
            self.enforcer,
        ))
    }
}
