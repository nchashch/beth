//! Wraps reth's stock Ethereum [`ConfigureEvm`] to additionally run two BIP300/301 "system
//! calls" once per block — the same mechanism EIP-4788/2935/7002 use to write protocol-level
//! state with no transaction and no signature (see `evm.transact_system_call` below):
//! - [`deposit_vault::deposits_calldata`] against `DepositVault`, crediting BIP300 deposits
//!   (Bitcoin mainchain -> this sidechain). See [`crate::deposit_vault`]'s module doc comment.
//! - [`withdrawal_bundle::compute_lifecycle_update`] against `WithdrawalRequestQueue`, advancing
//!   the BIP300 withdrawal-bundle lifecycle (this sidechain -> Bitcoin mainchain). See
//!   [`crate::withdrawal_bundle`]'s module doc comment.
//!
//! This hooks into [`BlockExecutor::apply_pre_execution_changes`], which runs once per block —
//! for a block being *built* (via [`ConfigureEvm::context_for_next_block`]) and, identically,
//! for a block being *validated* (via [`ConfigureEvm::context_for_block`], which every node
//! runs on every block, self-produced or received, before accepting it). Because both calls are
//! pure functions of already-agreed inputs (each contract's own prior state, plus the enforcer's
//! report of mainchain events for this block's own committed range), every node computes the
//! identical system calls independently — so if a block's declared `stateRoot` doesn't match
//! what a validator computes, standard state-root validation (already inherited via
//! `EthBeaconConsensus`) rejects it. No separate consensus check needed for either.

use std::fmt::Debug;

use alloy_consensus::Header;
use alloy_eips::eip4788::SYSTEM_ADDRESS;
use alloy_evm::{
    Evm, EvmFactory, FromRecoveredTx, FromTxWithEncoded,
    block::{
        BlockExecutionError, BlockExecutionResult, BlockExecutor, BlockExecutorFactory,
        ExecutableTx, GasOutput, StateDB,
    },
    eth::{EthBlockExecutionCtx, EthBlockExecutorFactory, spec::EthExecutorSpec},
};
use alloy_primitives::B256;
use alloy_rpc_types_engine::ExecutionData;
use reth_chainspec::EthChainSpec;
use reth_ethereum_primitives::{Block, EthPrimitives, TransactionSigned};
use reth_evm::{
    ConfigureEngineEvm, ConfigureEvm, EvmEnv, EvmEnvFor, ExecutableTxIterator, ExecutionCtxFor,
    NextBlockEnvAttributes, precompiles::PrecompilesMap,
};
use reth_evm_ethereum::{EthBlockAssembler, EthEvmConfig, RethReceiptBuilder};
use reth_primitives_traits::{AlloyBlockHeader, SealedBlock, SealedHeader};
use reth_storage_api::{HeaderProvider, StateProviderFactory};
use revm::{
    DatabaseCommit, Inspector, context::Block as RevmBlockEnv, primitives::hardfork::SpecId,
};

use crate::{deposit_vault, enforcer::EnforcerClient, withdrawal_bundle};

/// Wraps [`EthEvmConfig`], swapping in [`Bip300301BlockExecutorFactory`] as the executor
/// factory. Every other [`ConfigureEvm`] method delegates straight to the inner config.
#[derive(Debug, Clone)]
pub struct Bip300301EvmConfig<ChainSpec, Provider, EvmF> {
    inner: EthEvmConfig<ChainSpec, EvmF>,
    factory: Bip300301BlockExecutorFactory<ChainSpec, Provider, EvmF>,
}

impl<ChainSpec, Provider, EvmF> Bip300301EvmConfig<ChainSpec, Provider, EvmF> {
    pub fn new(
        inner: EthEvmConfig<ChainSpec, EvmF>,
        enforcer: EnforcerClient,
        sidechain_id: u32,
        provider: Provider,
    ) -> Self
    where
        EvmF: Clone,
    {
        let factory = Bip300301BlockExecutorFactory {
            inner: inner.executor_factory.clone(),
            enforcer,
            sidechain_id,
            provider,
        };
        Self { inner, factory }
    }
}

impl<ChainSpec, Provider, EvmF> ConfigureEvm for Bip300301EvmConfig<ChainSpec, Provider, EvmF>
where
    ChainSpec:
        EthExecutorSpec + EthChainSpec<Header = Header> + reth_ethereum_forks::Hardforks + 'static,
    Provider: HeaderProvider<Header = Header>
        + StateProviderFactory
        + Clone
        + Debug
        + Send
        + Sync
        + Unpin
        + 'static,
    EvmF: EvmFactory<
            Tx: reth_evm::TransactionEnvMut
                    + FromRecoveredTx<TransactionSigned>
                    + FromTxWithEncoded<TransactionSigned>,
            Spec = SpecId,
            BlockEnv = revm::context::BlockEnv,
            Precompiles = PrecompilesMap,
        > + Clone
        + Debug
        + Send
        + Sync
        + Unpin
        + 'static,
{
    type Primitives = EthPrimitives;
    type Error = std::convert::Infallible;
    type NextBlockEnvCtx = NextBlockEnvAttributes;
    type BlockExecutorFactory = Bip300301BlockExecutorFactory<ChainSpec, Provider, EvmF>;
    type BlockAssembler = EthBlockAssembler<ChainSpec>;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        &self.factory
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        self.inner.block_assembler()
    }

    fn evm_env(&self, header: &Header) -> Result<EvmEnv<SpecId>, Self::Error> {
        self.inner.evm_env(header)
    }

    fn next_evm_env(
        &self,
        parent: &Header,
        attributes: &NextBlockEnvAttributes,
    ) -> Result<EvmEnv, Self::Error> {
        self.inner.next_evm_env(parent, attributes)
    }

    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<Block>,
    ) -> Result<EthBlockExecutionCtx<'a>, Self::Error> {
        self.inner.context_for_block(block)
    }

    fn context_for_next_block(
        &self,
        parent: &SealedHeader,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<EthBlockExecutionCtx<'_>, Self::Error> {
        self.inner.context_for_next_block(parent, attributes)
    }
}

/// Delegates straight to the inner [`EthEvmConfig`]'s implementation — required so this config
/// can still drive the engine API's payload-execution path (`newPayload`), which bypasses
/// [`ConfigureEvm::context_for_block`] and calls these methods directly instead. The BIP300
/// system call itself still runs via [`BlockExecutor::apply_pre_execution_changes`] regardless of
/// which path constructs the executor, so no bundle-lifecycle logic needs to live here.
impl<ChainSpec, Provider, EvmF> ConfigureEngineEvm<ExecutionData>
    for Bip300301EvmConfig<ChainSpec, Provider, EvmF>
where
    ChainSpec:
        EthExecutorSpec + EthChainSpec<Header = Header> + reth_ethereum_forks::Hardforks + 'static,
    Provider: HeaderProvider<Header = Header>
        + StateProviderFactory
        + Clone
        + Debug
        + Send
        + Sync
        + Unpin
        + 'static,
    EvmF: EvmFactory<
            Tx: reth_evm::TransactionEnvMut
                    + FromRecoveredTx<TransactionSigned>
                    + FromTxWithEncoded<TransactionSigned>,
            Spec = SpecId,
            BlockEnv = revm::context::BlockEnv,
            Precompiles = PrecompilesMap,
        > + Clone
        + Debug
        + Send
        + Sync
        + Unpin
        + 'static,
{
    fn evm_env_for_payload(&self, payload: &ExecutionData) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.inner.evm_env_for_payload(payload)
    }

    fn context_for_payload<'a>(
        &self,
        payload: &'a ExecutionData,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        self.inner.context_for_payload(payload)
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &ExecutionData,
    ) -> Result<impl ExecutableTxIterator<Self>, Self::Error> {
        self.inner.tx_iterator_for_payload(payload)
    }
}

/// Wraps [`EthBlockExecutorFactory`], producing [`Bip300301BlockExecutor`]s instead of plain
/// `EthBlockExecutor`s.
#[derive(Debug, Clone)]
pub struct Bip300301BlockExecutorFactory<ChainSpec, Provider, EvmF> {
    inner: EthBlockExecutorFactory<RethReceiptBuilder, std::sync::Arc<ChainSpec>, EvmF>,
    enforcer: EnforcerClient,
    sidechain_id: u32,
    provider: Provider,
}

impl<ChainSpec, Provider, EvmF> BlockExecutorFactory
    for Bip300301BlockExecutorFactory<ChainSpec, Provider, EvmF>
where
    ChainSpec: EthExecutorSpec + 'static,
    Provider: HeaderProvider<Header = Header>
        + StateProviderFactory
        + Clone
        + Debug
        + Send
        + Sync
        + Unpin
        + 'static,
    EvmF: EvmFactory<Tx: FromRecoveredTx<TransactionSigned> + FromTxWithEncoded<TransactionSigned>>
        + 'static,
{
    type EvmFactory = EvmF;
    type ExecutionCtx<'a> = EthBlockExecutionCtx<'a>;
    type Transaction = TransactionSigned;
    type Receipt = <EthBlockExecutorFactory<RethReceiptBuilder, std::sync::Arc<ChainSpec>, EvmF> as BlockExecutorFactory>::Receipt;
    type TxExecutionResult = <EthBlockExecutorFactory<
        RethReceiptBuilder,
        std::sync::Arc<ChainSpec>,
        EvmF,
    > as BlockExecutorFactory>::TxExecutionResult;
    type Executor<'a, DB: StateDB, I: Inspector<EvmF::Context<DB>>> =
        Bip300301BlockExecutor<
            'a,
            <EthBlockExecutorFactory<RethReceiptBuilder, std::sync::Arc<ChainSpec>, EvmF> as BlockExecutorFactory>::Executor<'a, DB, I>,
            Provider,
        >;

    fn evm_factory(&self) -> &Self::EvmFactory {
        self.inner.evm_factory()
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        evm: <Self::EvmFactory as EvmFactory>::Evm<DB, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> Self::Executor<'a, DB, I>
    where
        DB: StateDB,
        I: Inspector<EvmF::Context<DB>>,
    {
        Bip300301BlockExecutor {
            inner: self.inner.create_executor(evm, ctx.clone()),
            enforcer: self.enforcer.clone(),
            sidechain_id: self.sidechain_id,
            provider: self.provider.clone(),
            parent_hash: ctx.parent_hash,
            extra_data: ctx.extra_data,
            _marker: std::marker::PhantomData,
        }
    }
}

/// Wraps `EthBlockExecutor`, additionally running the BIP300 withdrawal-bundle system call in
/// [`apply_pre_execution_changes`](BlockExecutor::apply_pre_execution_changes). Every other
/// method delegates straight to the inner executor.
#[derive(Debug)]
pub struct Bip300301BlockExecutor<'a, Inner, Provider> {
    inner: Inner,
    enforcer: EnforcerClient,
    sidechain_id: u32,
    provider: Provider,
    parent_hash: B256,
    extra_data: alloy_primitives::Bytes,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl<Inner, Provider> BlockExecutor for Bip300301BlockExecutor<'_, Inner, Provider>
where
    Inner: BlockExecutor,
    <Inner::Evm as Evm>::DB: DatabaseCommit,
    Provider: HeaderProvider<Header = Header> + StateProviderFactory + Send + Sync + Debug,
{
    type Transaction = Inner::Transaction;
    type Receipt = Inner::Receipt;
    type Evm = Inner::Evm;
    type Result = Inner::Result;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.inner.apply_pre_execution_changes()?;

        let block_number = self.inner.evm().block().number().saturating_to::<u64>();
        // Genesis carries no BIP301 provenance and has no prior queue state to act on.
        if block_number == 0 {
            return Ok(());
        }

        let parent_header = self
            .provider
            .header(self.parent_hash)
            .map_err(BlockExecutionError::other)?
            .ok_or_else(|| {
                BlockExecutionError::msg(format!("parent header {} not found", self.parent_hash))
            })?;
        let parent_main_hash = B256::try_from(parent_header.extra_data().as_ref()).ok();
        let main_hash = B256::try_from(self.extra_data.as_ref())
            .map_err(|_| BlockExecutionError::msg("block extraData must be exactly 32 bytes"))?;

        let deposits = self
            .enforcer
            .deposits(self.sidechain_id, parent_main_hash, main_hash)
            .map_err(BlockExecutionError::other)?;
        if let Some(calldata) = deposit_vault::deposits_calldata(&deposits) {
            let result = self
                .inner
                .evm_mut()
                .transact_system_call(
                    SYSTEM_ADDRESS,
                    deposit_vault::DEPOSIT_VAULT_ADDRESS,
                    calldata.into(),
                )
                .map_err(|err| BlockExecutionError::msg(err.to_string()))?;
            self.inner.evm_mut().db_mut().commit(result.state);
        }

        let state = self
            .provider
            .state_by_block_hash(self.parent_hash)
            .map_err(BlockExecutionError::other)?;
        let all_requests = withdrawal_bundle::read_all_requests(state.as_ref())
            .map_err(BlockExecutionError::other)?;

        let outcomes = self
            .enforcer
            .withdrawal_bundle_events(self.sidechain_id, parent_main_hash, main_hash)
            .map_err(BlockExecutionError::other)?;

        let update = withdrawal_bundle::compute_lifecycle_update(
            &all_requests,
            &outcomes,
            u32::try_from(block_number).unwrap_or(u32::MAX),
        );
        if !update.is_empty() {
            let result = self
                .inner
                .evm_mut()
                .transact_system_call(
                    SYSTEM_ADDRESS,
                    withdrawal_bundle::WITHDRAWAL_REQUEST_QUEUE_ADDRESS,
                    update.to_calldata().into(),
                )
                .map_err(|err| BlockExecutionError::msg(err.to_string()))?;
            self.inner.evm_mut().db_mut().commit(result.state);
        }

        Ok(())
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError> {
        self.inner.execute_transaction_without_commit(tx)
    }

    fn commit_transaction(&mut self, output: Self::Result) -> GasOutput {
        self.inner.commit_transaction(output)
    }

    fn finish(
        self,
    ) -> Result<(Self::Evm, BlockExecutionResult<Self::Receipt>), BlockExecutionError> {
        self.inner.finish()
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        self.inner.evm_mut()
    }

    fn evm(&self) -> &Self::Evm {
        self.inner.evm()
    }

    fn receipts(&self) -> &[Self::Receipt] {
        self.inner.receipts()
    }
}

/// Builder that wires [`Bip300301EvmConfig`] in as the node's EVM config, wrapping whatever
/// [`reth_node_ethereum::node::EthereumExecutorBuilder`] would otherwise produce (so JIT
/// support, sender-recovery caching, etc. all still work unchanged).
#[derive(Debug, Clone)]
pub struct Bip300301ExecutorBuilder {
    enforcer: EnforcerClient,
    sidechain_id: u32,
}

impl Bip300301ExecutorBuilder {
    pub fn new(enforcer: EnforcerClient, sidechain_id: u32) -> Self {
        Self {
            enforcer,
            sidechain_id,
        }
    }
}

impl<Types, Node> reth_node_builder::components::ExecutorBuilder<Node> for Bip300301ExecutorBuilder
where
    Types: reth_node_builder::node::NodeTypes<
            ChainSpec: reth_ethereum_forks::Hardforks
                           + EthExecutorSpec
                           + reth_chainspec::EthereumHardforks,
            Primitives = EthPrimitives,
        >,
    Node: reth_node_builder::node::FullNodeTypes<Types = Types>,
{
    type EVM = Bip300301EvmConfig<
        Types::ChainSpec,
        Node::Provider,
        reth_evm_ethereum::factory::RethEvmFactory,
    >;

    async fn build_evm(
        self,
        ctx: &reth_node_builder::BuilderContext<Node>,
    ) -> eyre::Result<Self::EVM> {
        let inner = reth_node_ethereum::node::EthereumExecutorBuilder::default()
            .build_evm(ctx)
            .await?;
        Ok(Bip300301EvmConfig::new(
            inner,
            self.enforcer,
            self.sidechain_id,
            ctx.provider().clone(),
        ))
    }
}
