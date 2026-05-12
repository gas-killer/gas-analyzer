use anyhow::Result;
use clap::{Parser, Subcommand};
use indexer_service::{config, head_tracker, refresher, worker};
use tracing_subscriber::EnvFilter;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(name = "indexer-service", about = "Gas-killer block indexer")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Poll the chain head and fan out one job per qualifying tx.
    HeadTracker {
        #[command(flatten)]
        common: config::CommonConfig,
        #[command(flatten)]
        head: config::HeadTrackerConfig,
    },
    /// Consume jobs from Redis and persist analysis results.
    Worker {
        #[command(flatten)]
        common: config::CommonConfig,
        #[command(flatten)]
        worker: config::WorkerConfig,
    },
    /// Periodic background tasks (resolver, price, rollups).
    Refresher {
        #[command(flatten)]
        common: config::CommonConfig,
        #[command(flatten)]
        refresher: config::RefresherConfig,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(true)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::HeadTracker { common, head } => head_tracker::run(common, head).await,
        Command::Worker { common, worker } => worker::run(common, worker).await,
        Command::Refresher { common, refresher } => refresher::run(common, refresher).await,
    }
}
