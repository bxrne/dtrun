use clap::{Args, Parser, Subcommand};
use std::fmt;
use std::path::PathBuf;

/// dtrun: Deterministic execution for OCI containers.
#[derive(Parser, Debug)]
#[command(name = "dtrun", version, about, long_about = None)]
pub struct Cli {
    /// Global PRNG seed for deterministic randomness and scheduling
    #[arg(short, long, global = true, default_value_t = 42)]
    pub seed: u64,

    /// The subcommand to execute
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Create a container from an OCI bundle
    Create(ContainerArgs),

    /// Start execution of a previously created container
    Run(ContainerArgs),
}

impl fmt::Display for Commands {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Commands::Create(args) => write!(f, "CREATE container '{}'", args.id),
            Commands::Run(args) => write!(f, "RUN container '{}'", args.id),
        }
    }
}

/// Standard arguments shared by OCI-compliant commands
#[derive(Args, Debug)]
pub struct ContainerArgs {
    /// Unique identifier for the container instance
    #[arg(value_name = "CONTAINER_ID")]
    pub id: String,

    /// Path to the OCI bundle directory containing 'config.json'
    #[arg(short, long, value_name = "PATH", default_value = ".")]
    pub bundle: PathBuf,
}
