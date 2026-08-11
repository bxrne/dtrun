//! OCI conformance runner.
//!
//! Builds a scratch bundle that runs `runtimetest` (from
//! `opencontainers/runtime-tools`) inside a container and validates the emitted
//! TAP stream, failing on any `not ok` line.

use crate::oci::config::OciConfig;
use crate::runtime::Host;
use crate::runtime::host::HostError;
use std::fmt;
use std::path::{Path, PathBuf};

/// The TAP summary parsed from a container's runtimetest output.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub total: usize,
    pub pass: usize,
    pub skip: usize,
    pub fail: usize,
    /// The text of every failing (`not ok`) test.
    pub failures: Vec<String>,
}

/// Errors from the conformance runner.
#[derive(Debug)]
pub enum ConformanceError {
    /// `runtimetest` could not be located on this machine.
    RuntimetestNotFound,
    /// The source bundle is unusable (e.g. no `rootfs`).
    Bundle(String),
    Io(std::io::Error),
    Config(crate::oci::config::ConfigError),
    Runtime(HostError),
}

impl fmt::Display for ConformanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConformanceError::RuntimetestNotFound => write!(
                f,
                "runtimetest not found; install with `go install \
                 github.com/opencontainers/runtime-tools/cmd/runtimetest@master` \
                 or set $RUNTIMETEST_BIN"
            ),
            ConformanceError::Bundle(e) => write!(f, "bundle error: {e}"),
            ConformanceError::Io(e) => write!(f, "io error: {e}"),
            ConformanceError::Config(e) => write!(f, "{e}"),
            ConformanceError::Runtime(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ConformanceError {}

impl From<std::io::Error> for ConformanceError {
    fn from(value: std::io::Error) -> Self {
        ConformanceError::Io(value)
    }
}

impl From<crate::oci::config::ConfigError> for ConformanceError {
    fn from(value: crate::oci::config::ConfigError) -> Self {
        ConformanceError::Config(value)
    }
}

impl From<HostError> for ConformanceError {
    fn from(value: HostError) -> Self {
        ConformanceError::Runtime(value)
    }
}

/// Locate the `runtimetest` binary: `$RUNTIMETEST_BIN`, `$GOBIN`, `~/go/bin`,
/// then `$PATH`.
pub fn find_runtimetest() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("RUNTIMETEST_BIN") {
        let p = PathBuf::from(path);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(gobin) = std::env::var("GOBIN") {
        let p = PathBuf::from(gobin).join("runtimetest");
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home).join("go/bin/runtimetest");
        if p.is_file() {
            return Some(p);
        }
    }
    std::env::var("PATH")
        .ok()?
        .split(':')
        .map(|dir| PathBuf::from(dir).join("runtimetest"))
        .find(|p| p.is_file())
}

/// Run the OCI conformance suite against the given bundle's rootfs.
///
/// Copies the rootfs into a scratch bundle, injects `runtimetest`, runs the
/// container, and returns the parsed TAP [`Summary`]. The scratch bundle and
/// state directory are removed on success (or when `keep` is false).
pub fn run(bundle: &Path, seed: u64, keep: bool) -> Result<Summary, ConformanceError> {
    let runtimetest = find_runtimetest().ok_or(ConformanceError::RuntimetestNotFound)?;
    let scratch = make_bundle(bundle, &runtimetest)?;
    let state_root = std::env::temp_dir().join(format!(
        "dtrun-conformance-state-{}-{}",
        std::process::id(),
        unique(),
    ));

    let result = run_against(&scratch, &state_root, seed);

    if keep {
        tracing::info!(
            bundle = %scratch.display(),
            state = %state_root.display(),
            "conformance artifacts kept for inspection"
        );
    } else {
        let _ = std::fs::remove_dir_all(&scratch);
        let _ = std::fs::remove_dir_all(&state_root);
    }
    result
}

/// Copy the rootfs from `src_bundle`, add `runtimetest`, and write a
/// conformance `config.json` (both at the bundle root and inside the rootfs,
/// which is where runtimetest looks for it).
fn make_bundle(src_bundle: &Path, runtimetest: &Path) -> Result<PathBuf, ConformanceError> {
    let src_rootfs = src_bundle.join("rootfs");
    if !src_rootfs.is_dir() {
        return Err(ConformanceError::Bundle(format!(
            "no rootfs at {} (build the busybox rootfs first)",
            src_rootfs.display()
        )));
    }

    let dir = std::env::temp_dir().join(format!(
        "dtrun-conformance-{}-{}",
        std::process::id(),
        unique(),
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let rootfs = dir.join("rootfs");
    copy_dir(&src_rootfs, &rootfs)?;
    std::fs::copy(runtimetest, rootfs.join("runtimetest"))?;

    let config = conformance_config();
    let config_json = serde_json::to_string_pretty(&config)
        .map_err(|e| ConformanceError::Io(std::io::Error::other(e)))?;
    std::fs::write(dir.join("config.json"), &config_json)?;
    std::fs::write(rootfs.join("config.json"), &config_json)?;

    Ok(dir)
}

fn run_against(scratch: &Path, state_root: &Path, seed: u64) -> Result<Summary, ConformanceError> {
    let config = OciConfig::from_path(scratch.join("config.json"))?;
    let host = Host::new(config, scratch.to_path_buf(), seed);
    let code = host.run(state_root, "conformance")?;

    let trace =
        std::fs::read_to_string(state_root.join("conformance/trace.jsonl")).unwrap_or_default();
    let mut summary = tap_summary(&trace);
    if code != 0 && summary.failures.is_empty() {
        // runtimetest exited before emitting any TAP (setup failure).
        summary
            .failures
            .push(format!("container exited with status {code}"));
    }
    summary.fail = summary.failures.len();
    summary.total = summary.pass + summary.skip + summary.fail;
    Ok(summary)
}

/// Parse the TAP stream recorded in a container trace into a [`Summary`].
pub fn tap_summary(trace: &str) -> Summary {
    let mut pass = 0usize;
    let mut skip = 0usize;
    let mut failures = Vec::new();

    for line in trace.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v["event"].as_str() != Some("stdout") {
            continue;
        }
        let Some(text) = v["line"].as_str() else {
            continue;
        };
        let text = text.trim_end();
        if let Some(rest) = text.strip_prefix("ok ") {
            if rest.contains("# SKIP") {
                skip += 1;
            } else {
                pass += 1;
            }
        } else if text.starts_with("not ok") {
            failures.push(text.to_owned());
        }
    }

    Summary {
        total: pass + skip + failures.len(),
        pass,
        skip,
        fail: failures.len(),
        failures,
    }
}

/// The conformance `config.json`: a bundle that exercises every runtime feature
/// dtrun implements. Values that depend on the host (id mappings, device modes,
/// the inheritable `oom_score_adj`) are computed from the current process.
fn conformance_config() -> serde_json::Value {
    let device_mode = |name: &str| {
        std::fs::metadata(format!("/dev/{name}"))
            .map(|m| {
                use std::os::unix::fs::MetadataExt;
                m.mode() & 0o777
            })
            .unwrap_or(0o666)
    };
    let device = |path: &str, major: i64, minor: i64| {
        let name = path.rsplit('/').next().unwrap_or(path);
        serde_json::json!({
            "path": path,
            "type": "c",
            "major": major,
            "minor": minor,
            "fileMode": device_mode(name),
        })
    };

    let uid = nix::unistd::getuid().as_raw();
    let gid = nix::unistd::getgid().as_raw();

    // Rootless runtimes can only *raise* their own oom_score_adj (lowering it
    // needs CAP_SYS_RESOURCE in the initial user namespace), so the conformance
    // value must sit above whatever this process inherited.
    let oom_adj = std::fs::read_to_string("/proc/self/oom_score_adj")
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .map(|v| (v + 1).clamp(-1000, 1000))
        .unwrap_or(10);

    serde_json::json!({
        "ociVersion": "1.0.0",
        "root": { "path": "rootfs", "readonly": false },
        "hostname": "dtrun-runtimetest",
        "linux": {
            "devices": [
                device("/dev/null", 1, 3),
                device("/dev/zero", 1, 5),
                device("/dev/full", 1, 7),
                device("/dev/random", 1, 8),
                device("/dev/urandom", 1, 9),
                device("/dev/tty", 5, 0),
            ],
            "maskedPaths": [
                "/proc/kcore",
                "/proc/latency_stats",
                "/proc/timer_list",
                "/proc/sched_debug",
                "/sys/firmware",
                "/sys/devices/virtual/powercap",
            ],
            "readonlyPaths": [
                "/proc/sys",
                "/proc/irq",
                "/proc/bus",
                "/proc/fs",
            ],
            "sysctl": { "net.ipv4.ip_forward": "1" },
            "uidMappings": [{ "containerID": 0, "hostID": uid, "size": 1 }],
            "gidMappings": [{ "containerID": 0, "hostID": gid, "size": 1 }],
        },
        "mounts": [
            { "destination": "/proc", "type": "proc", "source": "proc",
              "options": ["nosuid", "noexec", "nodev", "rw"] },
            { "destination": "/dev", "type": "tmpfs", "source": "tmpfs",
              "options": ["nosuid", "strictatime", "mode=755", "size=65536k"] },
            { "destination": "/dev/pts", "type": "devpts", "source": "devpts",
              "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620"] },
            { "destination": "/sys", "type": "sysfs", "source": "sysfs",
              "options": ["nosuid", "noexec", "nodev", "ro"] },
            { "destination": "/dev/mqueue", "type": "mqueue", "source": "mqueue",
              "options": ["nosuid", "noexec", "nodev"] },
            { "destination": "/dev/shm", "type": "tmpfs", "source": "shm",
              "options": ["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"] }
        ],
        "process": {
            "args": ["/runtimetest"],
            "env": ["PATH=/usr/bin:/bin"],
            "cwd": "/",
            "oomScoreAdj": oom_adj,
            "capabilities": {
                "bounding": [],
                "effective": [],
                "inheritable": [],
                "permitted": [],
                "ambient": []
            }
        }
    })
}

/// Timestamp-based unique suffix for scratch directories.
fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Recursively copy a directory, preserving symlinks.
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_dir(&src, &dst)?;
        } else if ft.is_symlink() {
            let target = std::fs::read_link(&src)?;
            let _ = std::fs::remove_file(&dst);
            std::os::unix::fs::symlink(&target, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}
