use clap::Parser;
use tracing::{debug, error, info};
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Commands};
use crate::oci::config::OciConfig;

mod cli;
mod oci;

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

            // Load the OCI configuration from the specified bundle path
            let config_path = args.bundle.join("config.json");
            let config: OciConfig = match std::fs::read_to_string(&config_path) {
                Ok(content) => match serde_json::from_str(&content) {
                    Ok(cfg) => cfg,
                    Err(e) => {
                        error!("Failed to parse config.json: {}", e);
                        return;
                    }
                },
                Err(e) => {
                    error!("Failed to read config.json: {}", e);
                    return;
                }
            };

            info!("Loaded OCI configuration: {:?}", config);
        }
        Commands::Run(args) => {
            info!("Running container with ID: {}", args.id);
            debug!("Bundle path: {:?}", args.bundle);
        }
    }
}
