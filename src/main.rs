use reth_ethereum_cli::interface::Cli;
use reth_node_ethereum::{EthereumNode, node::EthereumAddOns};

fn main() -> eyre::Result<()> {
    Cli::parse_args().run(async move |builder, _| {
        let handle = builder
            .with_types::<EthereumNode>()
            .with_components(EthereumNode::components())
            .with_add_ons(EthereumAddOns::default())
            .launch()
            .await?;
        handle.wait_for_node_exit().await
    })
}
