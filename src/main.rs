use clap::Parser;
use std::fmt::Display;
use std::io::Write;
use std::path::Path;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use dtrun::cli::{Cli, Commands, CreateArgs, ExecArgs, FlattenArgs, RunArgs, SpecArgs};
use dtrun::oci::config::OciConfig;
use dtrun::oci::image;
use dtrun::runtime::{Host, NetMode, host};

fn main() {
    let cli = Cli::parse();

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let log_path = cli.log.clone();
    let fmt = tracing_subscriber::fmt().json().with_env_filter(filter);
    let fmt = fmt.with_writer(move || -> Box<dyn std::io::Write + Send> {
        match &log_path {
            Some(path) => {
                let f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .expect("failed to open log file");
                Box::new(f)
            }
            None => Box::new(std::io::stderr()),
        }
    });
    fmt.init();

    let root = cli.root_dir();
    let code = match &cli.command {
        Commands::Create(args) => cmd_create(&cli, &root, args),
        Commands::Start(args) => cmd_start(&root, &args.id),
        Commands::Run(args) => cmd_run(&cli, &root, args),
        Commands::Kill(args) => cmd_kill(&root, &args.id, &args.signal),
        Commands::Delete(args) => cmd_delete(&root, &args.id, args.force),
        Commands::State(args) => cmd_state(&root, &args.id),
        Commands::List(args) => cmd_list(&root, &args.format),
        Commands::Exec(args) => cmd_exec(&root, args),
        Commands::Spec(args) => cmd_spec(args),
        Commands::Flatten(args) => cmd_flatten(args),
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

fn cmd_create(cli: &Cli, root: &Path, args: &CreateArgs) -> i32 {
    let bundle = args.bundle.bundle.clone();
    let host = match load_host(&bundle, cli.seed, &args.bundle.net) {
        Ok(h) => h,
        Err(e) => return fail(e),
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

fn cmd_start(root: &Path, id: &str) -> i32 {
    match host::start_container(root, id) {
        Ok(()) => {
            info!(id, "started");
            0
        }
        Err(e) => fail(e),
    }
}

fn cmd_run(cli: &Cli, root: &Path, args: &RunArgs) -> i32 {
    let bundle = args.bundle.bundle.clone();
    let host = match load_host(&bundle, cli.seed, &args.bundle.net) {
        Ok(h) => h,
        Err(e) => return fail(e),
    };
    match host.run(root, &args.bundle.id) {
        Ok(code) => {
            info!(id = %args.bundle.id, exit_code = code, "run finished");
            code
        }
        Err(e) => fail(e),
    }
}

fn cmd_kill(root: &Path, id: &str, signal: &str) -> i32 {
    match host::kill_container(root, id, signal) {
        Ok(()) => {
            info!(id, signal, "signal sent");
            0
        }
        Err(e) => fail(e),
    }
}

fn cmd_delete(root: &Path, id: &str, force: bool) -> i32 {
    match host::delete_container(root, id, force) {
        Ok(()) => {
            info!(id, "deleted");
            0
        }
        Err(e) => fail(e),
    }
}

fn cmd_state(root: &Path, id: &str) -> i32 {
    match host::print_state(root, id) {
        Ok(()) => 0,
        Err(e) => fail(e),
    }
}

fn cmd_list(root: &Path, format: &str) -> i32 {
    match host::list_containers(root, format) {
        Ok(()) => 0,
        Err(e) => fail(e),
    }
}

fn cmd_exec(root: &Path, args: &ExecArgs) -> i32 {
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

fn cmd_spec(args: &SpecArgs) -> i32 {
    let config = default_config_json();
    let path = args.bundle.join("config.json");
    let mut f = match std::fs::File::create(&path) {
        Ok(f) => f,
        Err(e) => return fail(format!("cannot write {}: {e}", path.display())),
    };
    if let Err(e) = writeln!(f, "{config}") {
        return fail(format!("cannot write {}: {e}", path.display()));
    }
    info!(path = %path.display(), "wrote default config");
    0
}

fn cmd_flatten(args: &FlattenArgs) -> i32 {
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
