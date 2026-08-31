mod consensus;
mod enforcer;
mod payload;
mod proto;

use consensus::Bip300301ConsensusBuilder;
use enforcer::EnforcerClient;
use payload::Bip300301PayloadBuilderBuilder;
use reth_ethereum_cli::interface::Cli;
use reth_node_builder::components::BasicPayloadServiceBuilder;
use reth_node_ethereum::{EthereumNode, node::EthereumAddOns};

fn main() -> eyre::Result<()> {
    Cli::parse_args().run(async move |builder, _| {
        // TODO: make configurable (CLI flag / env var) instead of hardcoding.
        let enforcer_url = std::env::var("BIP300301_ENFORCER_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8080".into());
        let enforcer = EnforcerClient::new(enforcer_url);

        // TODO: make configurable (CLI flag / env var) instead of hardcoding. This is the
        // sidechain slot number beth registers as with the enforcer.
        let sidechain_id: u32 = std::env::var("BETH_SIDECHAIN_ID")
            .ok()
            .map(|s| s.parse())
            .transpose()?
            .unwrap_or(0);

        let handle = builder
            .with_types::<EthereumNode>()
            .with_components(
                EthereumNode::components()
                    .consensus(Bip300301ConsensusBuilder::new(
                        enforcer.clone(),
                        sidechain_id,
                    ))
                    .payload(BasicPayloadServiceBuilder::new(
                        Bip300301PayloadBuilderBuilder::new(enforcer, sidechain_id),
                    )),
            )
            .with_add_ons(EthereumAddOns::default())
            .launch()
            .await?;
        handle.wait_for_node_exit().await
    })
}
