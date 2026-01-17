//! Binary entrypoint for the Casper node proxy.

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    casper_node_proxy::run().await
}
