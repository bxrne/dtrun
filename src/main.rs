use clap::Parser;
use tracing::{debug, error, info};
use tracing_subscriber::EnvFilter;

use dtrun::oci::config::OciConfig;

mod cli;

use crate::cli::{Cli, Commands};

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

            let config_path = args.bundle.join("config.json");
            match OciConfig::from_path(&config_path) {
                Ok(config) => {
                    info!("Loaded OCI configuration: {:?}", config);
                }
                Err(e) => {
                    error!("{}", e);
                }
            }
        }
        Commands::Run(args) => {
            info!("Running container with ID: {}", args.id);
            debug!("Bundle path: {:?}", args.bundle);
        }
    }
}
