//! Command-line interface, following the OCI runtime command-line
//! conventions ([runtime-spec](https://github.com/opencontainers/runtime-spec)).

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// dtrun: Deterministic execution for OCI containers.
#[derive(Parser, Debug)]
#[command(
    name = "dtrun",
    version,
    about = "Deterministic OCI container runtime",
    long_about = None
)]
pub struct Cli {
    /// Root directory for container state (OCI `state.root`).
    /// Defaults to `$XDG_RUNTIME_DIR/dtrun` when unprivileged, else `/run/dtrun`.
    #[arg(long, global = true, value_name = "PATH")]
    pub root: Option<PathBuf>,

    /// Seed for the deterministic PRNG and virtual clock
    #[arg(long, global = true, default_value_t = 42)]
    pub seed: u64,

    /// Write runtime logs to FILE (default: stderr)
    #[arg(long, global = true, value_name = "FILE")]
    pub log: Option<PathBuf>,

    /// Subcommand to execute
    #[command(subcommand)]
    pub command: Commands,
}

impl Cli {
    /// Resolve the state root: an explicit `--root`, else
    /// `$XDG_RUNTIME_DIR/dtrun`, else `/run/dtrun` if writable, else a
    /// uid-scoped temp dir. The chosen root is created if possible.
    pub fn root_dir(&self) -> PathBuf {
        if let Some(p) = &self.root {
            return p.clone();
        }
        if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR")
            && !dir.is_empty()
        {
            return PathBuf::from(dir).join("dtrun");
        }
        let run = PathBuf::from("/run/dtrun");
        if std::fs::create_dir_all(&run).is_ok() {
            return run;
        }
        std::env::temp_dir().join(format!("dtrun-{}", unsafe { libc::getuid() }))
    }
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Create a container: parse the bundle and set up isolation, but do not start it
    Create(CreateArgs),
    /// Start a previously created container
    Start(StartArgs),
    /// Create, start, wait for, and delete a container in one shot
    Run(RunArgs),
    /// Send a signal to the container's init process
    Kill(KillArgs),
    /// Delete the state of a container (kills it unless --force handling applies)
    Delete(DeleteArgs),
    /// Print the container's OCI state as JSON
    State(StateArgs),
    /// List containers known to the runtime
    List(ListArgs),
    /// Execute a command inside a running container
    Exec(ExecArgs),
    /// Generate a default OCI `config.json`
    Spec(SpecArgs),
    /// Flatten an OCI/Docker image tarball into a deterministic rootfs
    Flatten(FlattenArgs),
    /// Print version information
    Version,
}

/// Shared arguments for commands that create a container from a bundle.
#[derive(Args, Debug)]
pub struct BundleArgs {
    /// Unique identifier for the container
    #[arg(value_name = "CONTAINER_ID")]
    pub id: String,

    /// Path to the OCI bundle directory containing `config.json`
    #[arg(short, long, value_name = "PATH", default_value = ".")]
    pub bundle: PathBuf,

    /// Write the container's PID to this file
    #[arg(long, value_name = "FILE")]
    pub pid_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct CreateArgs {
    #[command(flatten)]
    pub bundle: BundleArgs,
}

#[derive(Args, Debug)]
pub struct StartArgs {
    /// Unique identifier for the container
    #[arg(value_name = "CONTAINER_ID")]
    pub id: String,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    #[command(flatten)]
    pub bundle: BundleArgs,
}

#[derive(Args, Debug)]
pub struct KillArgs {
    /// Unique identifier for the container
    #[arg(value_name = "CONTAINER_ID")]
    pub id: String,

    /// Signal to send (default: SIGTERM)
    #[arg(value_name = "SIGNAL", default_value = "SIGTERM")]
    pub signal: String,
}

#[derive(Args, Debug)]
pub struct DeleteArgs {
    /// Unique identifier for the container
    #[arg(value_name = "CONTAINER_ID")]
    pub id: String,

    /// Delete even if the container is running (sends SIGKILL)
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct StateArgs {
    /// Unique identifier for the container
    #[arg(value_name = "CONTAINER_ID")]
    pub id: String,
}

#[derive(Args, Debug)]
pub struct ListArgs {
    /// Output format: table (default) or json
    #[arg(long, value_name = "FORMAT", default_value = "table")]
    pub format: String,
}

#[derive(Args, Debug)]
pub struct ExecArgs {
    /// Unique identifier for the container
    #[arg(value_name = "CONTAINER_ID")]
    pub id: String,

    /// Command to run inside the container
    #[arg(value_name = "COMMAND", required = true)]
    pub command: Vec<String>,

    /// Working directory inside the container
    #[arg(long, value_name = "PATH")]
    pub cwd: Option<String>,

    /// Environment variables in KEY=VALUE form
    #[arg(long, value_name = "KEY=VALUE")]
    pub env: Vec<String>,
}

#[derive(Args, Debug)]
pub struct SpecArgs {
    /// Directory to write `config.json` into
    #[arg(value_name = "PATH", default_value = ".")]
    pub bundle: PathBuf,
}

#[derive(Args, Debug)]
pub struct FlattenArgs {
    /// Path to the image tarball (`.tar` or `.tar.gz`)
    #[arg(value_name = "IMAGE")]
    pub image: PathBuf,

    /// Directory to flatten the rootfs into
    #[arg(value_name = "DEST")]
    pub dest: PathBuf,
}
