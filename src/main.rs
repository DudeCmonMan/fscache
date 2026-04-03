mod cache_manager;
mod cacher;
mod config;
mod plex_api;
mod plex_db;
mod predictor;
mod utils;

use clap::Parser;
use std::path::PathBuf;

const BUILD_VERSION: &str = env!("BUILD_VERSION");

#[derive(Parser, Debug)]
#[command(
    name = "plex-hot-cache",
    version = BUILD_VERSION,
    about = "Predictive SSD caching for Plex media via MergerFS"
)]
struct Args {
    /// Path to config file
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("plex-hot-cache {} starting", BUILD_VERSION);

    let _args = Args::parse();

    Ok(())
}
