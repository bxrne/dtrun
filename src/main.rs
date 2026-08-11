use clap::Parser;
use std::fmt::Display;
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use libdtrun::cli::{Cli, Commands, FlattenArgs};
use libdtrun::oci::config::OciConfig;
use libdtrun::oci::image;
use libdtrun::runtime::{Host, NetMode, host};

// Initialize tracing with optional log file path.
fn init_tracing(log_path: Option<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt = tracing_subscriber::fmt().json().with_env_filter(filter);
    let fmt = fmt.with_writer(move || -> Box<dyn std::io::Write + Send> {
        match &log_path {
            Some(path) => match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                Ok(f) => Box::new(f),
                Err(e) => {
                    eprintln!("failed to open log file {path:?}: {e}; falling back to stderr");
                    Box::new(std::io::stderr())
                }
            },
            None => Box::new(std::io::stderr()),
        }
    });
    fmt.init();
    Ok(())
}

fn main() {
    let cli = Cli::parse();

    match init_tracing(cli.log.clone()) {
        Err(e) => {
            error!("failed to initialize tracing: {e}");
            std::process::exit(1);
        }
        Ok(()) => {}
    }

    let root: &Path = &cli.root_dir();
    let code = match &cli.command {
        Commands::Create(args) => {
            let host = match load_host(&args.bundle.bundle, cli.seed, &args.bundle.net) {
                Ok(h) => h,
                Err(e) => std::process::exit(fail(e)),
            };
            match host.create(root, &args.bundle.id) {
                Ok(pid) => {
                    write_pid_file(args.bundle.pid_file.as_deref(), pid.as_raw());
                    info!(id = %args.bundle.id, pid = pid.as_raw(), "created");
                    0
                }

                Err(e) => fail(e),
            }
        }
        Commands::Start(args) => match host::start_container(root, args.id.as_str()) {
            Ok(()) => {
                info!(id = %args.id, "started");
                0
            }
            Err(e) => fail(e),
        },
        Commands::Run(args) => {
            let host = match load_host(&args.bundle.bundle, cli.seed, &args.bundle.net) {
                Ok(h) => h,
                Err(e) => std::process::exit(fail(e)),
            };
            match host.run(root, &args.bundle.id) {
                Ok(code) => {
                    info!(id = %args.bundle.id, exit_code = code, "run finished");
                    code
                }
                Err(e) => fail(e),
            }
        }
        Commands::Kill(args) => match host::kill_container(root, &args.id, &args.signal) {
            Ok(()) => {
                info!(id = %args.id, signal = %args.signal, "killed");
                0
            }
            Err(e) => fail(e),
        },
        Commands::Delete(args) => {
            let root: &Path = &root;
            let id: &str = &args.id;
            let force = args.force;
            match host::delete_container(root, id, force) {
                Ok(()) => {
                    info!(id, "deleted");
                    0
                }
                Err(e) => fail(e),
            }
        }
        Commands::State(args) => {
            let root: &Path = &root;
            let id: &str = &args.id;
            match host::print_state(root, id) {
                Ok(()) => 0,
                Err(e) => fail(e),
            }
        }
        Commands::List(args) => {
            let root: &Path = &root;
            let format: &str = &args.format;
            match host::list_containers(root, format) {
                Ok(()) => 0,
                Err(e) => fail(e),
            }
        }
        Commands::Exec(args) => {
            let root: &Path = &root;
            match host::exec_in_container(
                root,
                &args.id,
                &args.command,
                args.cwd.as_deref(),
                &args.env,
            ) {
                Ok(code) => code,
                Err(e) => fail(e),
            }
        }
        Commands::Spec(args) => {
            let config = default_config_json();
            let path = args.bundle.join("config.json");
            let mut f = match std::fs::File::create(&path) {
                Ok(f) => f,
                Err(e) => {
                    error!(path = %path.display(), ?e, "failed to create config.json");
                    std::process::exit(1);
                }
            };
            if let Err(e) = writeln!(f, "{config}") {
                error!(path = %path.display(), ?e, "failed to write config.json");
                std::process::exit(1);
            }
            info!(path = %path.display(), "wrote default config");
            0
        }
        Commands::Flatten(args) => {
            let args: &FlattenArgs = args;
            match image::flatten(&args.image, &args.dest) {
                Ok(()) => {
                    info!(
                        image = %args.image.display(),
                        dest = %args.dest.display(),
                        "flattened image deterministically"
                    );
                    0
                }
                Err(e) => fail(e),
            }
        }
        Commands::Version => {
            info!(
                version = env!("CARGO_PKG_VERSION"),
                oci_spec = "1.0.2",
                "dtrun version"
            );
            0
        }
    };

    std::process::exit(code);
}

fn load_host(bundle: &Path, seed: u64, net: &str) -> Result<Host, String> {
    let config_path = bundle.join("config.json");
    let config = OciConfig::from_path(&config_path)
        .map_err(|e| format!("failed to load {}: {e}", config_path.display()))?;
    let net_mode = NetMode::parse(net).map_err(|e| e.to_string())?;
    Ok(Host::with_net(config, bundle.to_path_buf(), seed, net_mode))
}

fn write_pid_file(path: Option<&std::path::Path>, pid: i32) {
    if let Some(path) = path
        && let Err(e) = std::fs::write(path, format!("{pid}\n"))
    {
        warn!(path = %path.display(), ?e, "failed to write pid file");
    }
}

fn fail(e: impl Display) -> i32 {
    error!("{e}");
    1
}

/// A minimal, spec-compliant default `config.json`.
fn default_config_json() -> String {
    let env: Vec<&str> = vec!["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"];
    let env = serde_json::json!(env);
    let mounts = serde_json::json!([
        {"destination": "/proc", "type": "proc", "source": "proc", "options": ["nosuid","noexec","nodev"]},
        {"destination": "/dev", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid","strictatime","mode=755","size=65536k"]},
        {"destination": "/dev/pts", "type": "devpts", "source": "devpts", "options": ["nosuid","noexec","newinstance","ptmxmode=0666","mode=0620","gid=5"]},
        {"destination": "/sys", "type": "sysfs", "source": "sysfs", "options": ["nosuid","noexec","nodev","ro"]},
        {"destination": "/dev/mqueue", "type": "mqueue", "source": "mqueue", "options": ["nosuid","noexec","nodev"]},
        {"destination": "/dev/shm", "type": "tmpfs", "source": "shm", "options": ["nosuid","noexec","nodev","mode=1777","size=65536k"]}
    ]);
    let namespaces = serde_json::json!([
        {"type": "pid"},
        {"type": "network"},
        {"type": "ipc"},
        {"type": "uts"},
        {"type": "mount"}
    ]);
    let config = serde_json::json!({
        "ociVersion": "1.0.2",
        "process": {
            "terminal": false,
            "user": {"uid": 0, "gid": 0},
            "args": ["/bin/sh"],
            "env": env,
            "cwd": "/"
        },
        "root": {"path": "rootfs", "readonly": false},
        "hostname": "dtrun",
        "mounts": mounts,
        "linux": {"namespaces": namespaces}
    });
    serde_json::to_string_pretty(&config).unwrap_or_else(|_| "{}".to_owned())
}
