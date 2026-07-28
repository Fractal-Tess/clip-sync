use clap::Parser;
use clip_sync::cli::Cli;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("clip_sync=info")),
        )
        .with_target(false)
        .compact()
        .init();

    Cli::parse().run().await
}
