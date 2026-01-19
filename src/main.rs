//! Binary entrypoint for the Casper node proxy.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "casper-node-proxy")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP proxy service.
    Start,
    /// Apply database migrations only.
    Migrate,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Start) {
        Command::Start => casper_node_proxy::run().await,
        Command::Migrate => casper_node_proxy::migrate().await,
    }
}
