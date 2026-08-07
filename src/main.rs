use crate::cli::{Cli, Commands};
use clap::Parser;
use tracing::{debug, info};
use tracing_subscriber::EnvFilter;
mod cli;

fn main() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Create(args) => {
            info!("Creating container with ID: {}", args.id);
            debug!("Bundle path: {:?}", args.bundle);
        }
        Commands::Run(args) => {
            info!("Running container with ID: {}", args.id);
            debug!("Bundle path: {:?}", args.bundle);
        }
    }
}
