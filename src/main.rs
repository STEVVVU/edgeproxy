use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use edgeproxy::{BoundProxy, Config};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[arg(short, long, default_value = "edgeproxy.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("edgeproxy=info")),
        )
        .json()
        .with_current_span(false)
        .with_span_list(false)
        .init();

    let cli = Cli::parse();
    let config = Config::from_path(&cli.config)?;
    let proxy = BoundProxy::bind(config).await?;
    proxy
        .run(async {
            if let Err(error) = tokio::signal::ctrl_c().await {
                tracing::error!(event = "signal_error", %error);
            }
        })
        .await
}
