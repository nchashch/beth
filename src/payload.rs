//! Payload builder that stamps each built block's `extraData` with the current BIP301
//! mainchain tip, so that [`crate::consensus::Bip300301Consensus`] has something to verify a BMM
//! commitment against once the block is BMM-mined into a Bitcoin block. It also mints BIP300
//! deposits (Bitcoin mainchain -> this sidechain) onto this sidechain, applied as EIP-4895
//! `Withdrawal`s (see [`next_block_context`]).
//!
//! Naming collision to watch for throughout this file: an EIP-4895 "withdrawal" is an Ethereum
//! protocol mechanism (originally: validator stake leaving the beacon chain) that we're
//! repurposing here purely as a vehicle for crediting balances outside normal transactions. It
//! is unrelated to — and points the opposite direction from — a BIP300 "withdrawal", which is a
//! sidechain-to-mainchain transfer (this sidechain -> Bitcoin mainchain), handled elsewhere via
//! `WithdrawalBundleEvent`/`GetCoinbasePSBT` on the enforcer, not implemented here at all.
//! Every `Withdrawal`/`withdrawals` below refers to the EIP-4895 (deposit-minting) sense.
//!
//! Note: this only handles minting deposits. `Bip300301Consensus` does not yet independently verify
//! that a block's withdrawals match the real deposits for the mainchain range it claims — a
//! malicious block producer could currently fabricate withdrawals. That verification is the
//! natural next piece of work here.

use std::sync::Arc;

// EIP-4895 `Withdrawal` — repurposed here to mint BIP300 deposits. See the module doc comment
// for why this is not a BIP300 withdrawal (which goes the other direction, L2 -> L1).
use alloy_eips::eip4895::Withdrawal;
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

use crate::enforcer::EnforcerClient;

type BestTransactionsIter<Pool> = Box<
    dyn BestTransactions<Item = Arc<ValidPoolTransaction<<Pool as TransactionPool>::Transaction>>>,
>;

/// 1 satoshi = 10 gwei = 10^10 wei, so that the L2 asset preserves BTC's 8 decimals of
/// precision within the usual 18-decimal wei denomination.
const GWEI_PER_SAT: u64 = 10;

/// Payload builder that delegates to [`default_ethereum_payload`], overriding `extraData` on
/// every build with the enforcer's current mainchain tip, and injecting BIP300 deposits
/// (Bitcoin mainchain -> this sidechain) made since the parent block, as EIP-4895 withdrawals —
/// not to be confused with BIP300 withdrawals (this sidechain -> Bitcoin mainchain).
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
    pub const fn new(
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

    /// Fetches the current mainchain tip and the BIP300 deposits (Bitcoin mainchain -> this
    /// sidechain) made since `parent_extra_data` (the parent block's own mainchain reference),
    /// returning the builder config with `extraData` set to the new tip, and those deposits
    /// converted to EIP-4895 withdrawals — the Ethereum-protocol mechanism, not a BIP300
    /// withdrawal (which would move value the other way, back to Bitcoin mainchain).
    ///
    /// A `parent_extra_data` that isn't a valid 32-byte mainchain hash (e.g. building directly
    /// on genesis, which carries no BIP301 provenance) is treated as "start of history".
    fn next_block_context(
        &self,
        parent_extra_data: &Bytes,
    ) -> Result<(EthereumBuilderConfig, Vec<Withdrawal>), PayloadBuilderError> {
        let main_tip = self
            .enforcer
            .chain_tip()
            .map_err(PayloadBuilderError::other)?;
        let parent_main_hash = B256::try_from(parent_extra_data.as_ref()).ok();

        let deposits = self
            .enforcer
            .deposits(self.sidechain_id, parent_main_hash, main_tip)
            .map_err(PayloadBuilderError::other)?;
        // Mint each BIP300 deposit as an EIP-4895 `Withdrawal` (Ethereum-protocol sense — this
        // credits the address directly, no BIP300 sidechain-to-mainchain transfer involved).
        let withdrawals = deposits
            .into_iter()
            .enumerate()
            .map(|(index, deposit)| Withdrawal {
                index: index as u64,
                validator_index: 0,
                address: deposit.address,
                amount: deposit.value_sats.saturating_mul(GWEI_PER_SAT),
            })
            .collect();

        let builder_config = self
            .builder_config
            .clone()
            .with_extra_data(Bytes::copy_from_slice(main_tip.as_slice()));

        Ok((builder_config, withdrawals))
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
        let (builder_config, withdrawals) =
            self.next_block_context(args.config.parent_header.header().extra_data())?;
        // `attributes.withdrawals` is the EIP-4895 field — here it carries BIP300 deposits, not
        // BIP300 withdrawals (see module doc comment).
        args.config.attributes.withdrawals = Some(withdrawals);

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
        let (builder_config, withdrawals) =
            self.next_block_context(config.parent_header.header().extra_data())?;
        // `attributes.withdrawals` is the EIP-4895 field — here it carries BIP300 deposits, not
        // BIP300 withdrawals (see module doc comment).
        config.attributes.withdrawals = Some(withdrawals);

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
