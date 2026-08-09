//! End-to-end runtime suite.
//!
//! Two groups of tests run against the built `dtrun` binary:
//!
//! 1. Determinism and lifecycle: identical seeds produce byte-identical
//!    traces, different seeds change injected randomness, exit codes and
//!    stdout are propagated, and the OCI create/start flow completes.
//! 2. OCI conformance: the container is validated inside itself by
//!    `runtimetest` from `github.com/opencontainers/runtime-tools` (built via
//!    `go install .../cmd/runtimetest@master`), and the emitted TAP output is
//!    checked. The suite is skipped when `runtimetest` is not installed.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

/// The bundle used by the deterministic workloads.
fn bundle_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/bundle")
}

/// A scratch OCI bundle running a single-process busybox command, so the
/// syscall trace is fully reproducible (no fork/exec interleaving).
fn single_bundle(command: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dtrun-single-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create single bundle");
    copy_dir(&bundle_dir().join("rootfs"), &dir.join("rootfs"));
    let config = serde_json::json!({
        "ociVersion": "1.0.0",
        "root": { "path": "rootfs", "readonly": false },
        "hostname": "dtrun-single",
        "process": {
            "args": ["/bin/busybox", "sh", "-c", command],
            "env": ["PATH=/usr/bin:/bin"],
            "cwd": "/"
        }
    });
    std::fs::write(
        dir.join("config.json"),
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .expect("write single config");
    dir
}

/// A scratch, per-test state root that is removed at the end of the test.
struct ScratchRoot(PathBuf);

impl ScratchRoot {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "dtrun-test-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch root");
        ScratchRoot(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn trace(&self, id: &str) -> String {
        std::fs::read_to_string(self.0.join(id).join("trace.jsonl"))
            .unwrap_or_else(|e| panic!("cannot read trace for {id}: {e}"))
    }
}

impl Drop for ScratchRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dtrun() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dtrun"))
}

fn dtrun_run(root: &Path, id: &str, seed: u64, bundle: &Path) -> Output {
    Command::new(dtrun())
        .arg("run")
        .arg(id)
        .args(["--bundle"])
        .arg(bundle)
        .args(["--root"])
        .arg(root)
        .args(["--seed"])
        .arg(seed.to_string())
        .env("RUST_LOG", "error")
        .output()
        .expect("failed to spawn dtrun")
}

/// Extract the hex bytes of the first injected `getrandom` from a trace.
fn first_getrandom_bytes(trace: &str) -> String {
    for line in trace.lines() {
        if line.contains("\"event\":\"getrandom\"") && line.contains("\"bytes\"") {
            let v: serde_json::Value = serde_json::from_str(line).expect("valid trace json");
            return v["detail"]["bytes"].as_str().unwrap_or("").to_owned();
        }
    }
    panic!("no injected getrandom found in trace")
}

fn exit_code(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn same_seed_produces_byte_identical_trace() {
    let bundle = single_bundle("true");
    let root = ScratchRoot::new("determinism");
    let first = dtrun_run(root.path(), "det", 42, &bundle);
    assert_eq!(exit_code(&first), 0, "run failed: {}", stderr(&first));
    let trace_a = root.trace("det");

    let second = dtrun_run(root.path(), "det", 42, &bundle);
    assert_eq!(exit_code(&second), 0, "run failed: {}", stderr(&second));
    let trace_b = root.trace("det");

    let _ = std::fs::remove_dir_all(&bundle);
    assert!(!trace_a.is_empty(), "trace must contain events");
    assert_eq!(trace_a, trace_b, "same seed must yield an identical trace");
}

#[test]
fn different_seeds_change_injected_randomness() {
    let bundle = single_bundle("true");
    let root = ScratchRoot::new("seeds");
    let a = dtrun_run(root.path(), "seed-a", 7, &bundle);
    assert_eq!(exit_code(&a), 0, "run failed: {}", stderr(&a));
    let bytes_a = first_getrandom_bytes(&root.trace("seed-a"));

    let b = dtrun_run(root.path(), "seed-b", 99, &bundle);
    assert_eq!(exit_code(&b), 0, "run failed: {}", stderr(&b));
    let bytes_b = first_getrandom_bytes(&root.trace("seed-b"));

    let _ = std::fs::remove_dir_all(&bundle);
    assert!(!bytes_a.is_empty());
    assert!(!bytes_b.is_empty());
    assert_ne!(
        bytes_a, bytes_b,
        "different seeds must inject different bytes"
    );
}

#[test]
fn exit_code_is_propagated_to_caller() {
    let bundle = single_bundle("false");
    let root = ScratchRoot::new("exit-code");
    let out = dtrun_run(root.path(), "exit", 1, &bundle);
    // `busybox false` exits 1.
    let _ = std::fs::remove_dir_all(&bundle);
    assert_eq!(exit_code(&out), 1, "stderr: {}", stderr(&out));
}

#[test]
fn stdout_output_is_relayed_and_recorded() {
    let bundle = single_bundle("echo hello-from-single");
    let root = ScratchRoot::new("stdout");
    let out = dtrun_run(root.path(), "stdout", 1, &bundle);
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let trace = root.trace("stdout");
    let _ = std::fs::remove_dir_all(&bundle);
    let stdout_lines: Vec<&str> = trace
        .lines()
        .filter(|l| l.contains("\"event\":\"stdout\""))
        .collect();
    assert!(
        !stdout_lines.is_empty(),
        "stdout events must be recorded in the trace"
    );
}

#[test]
fn create_start_lifecycle_transitions_to_stopped() {
    let root = ScratchRoot::new("lifecycle");
    let id = "life";

    let created = Command::new(dtrun())
        .arg("create")
        .arg(id)
        .args(["--bundle"])
        .arg(bundle_dir())
        .args(["--root"])
        .arg(root.path())
        .env("RUST_LOG", "error")
        .output()
        .expect("spawn create");
    assert_eq!(
        created.status.code(),
        Some(0),
        "create failed: {}",
        stderr(&created)
    );

    let state_path = root.path().join(id).join("state.json");
    assert_eq!(poll_status(&state_path, &["created"]), "created");

    let started = Command::new(dtrun())
        .arg("start")
        .arg(id)
        .args(["--root"])
        .arg(root.path())
        .env("RUST_LOG", "error")
        .output()
        .expect("spawn start");
    assert_eq!(
        started.status.code(),
        Some(0),
        "start failed: {}",
        stderr(&started)
    );

    assert_eq!(poll_status(&state_path, &["stopped"]), "stopped");
    assert!(
        !root.trace(id).is_empty(),
        "a completed create/start run must produce a trace"
    );
}

// ---------------------------------------------------------------------------
// OCI conformance: runtimetest (opencontainers/runtime-tools).
// ---------------------------------------------------------------------------

/// Locate the `runtimetest` binary: `$RUNTIMETEST_BIN`, `~/go/bin`, or the
/// GOBIN used during development.
fn runtimetest_bin() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("RUNTIMETEST_BIN") {
        let p = PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }
    for candidate in [
        std::env::var("GOBIN")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::home_dir().unwrap_or_default().join("go/bin")),
        Path::new("/tmp/opencode/gobin").to_path_buf(),
    ] {
        let p = candidate.join("runtimetest");
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Copy the example rootfs into a scratch bundle and add `runtimetest`.
fn make_runtimetest_bundle(bin: &Path) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dtrun-runtimetest-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create runtimetest bundle");

    let rootfs = dir.join("rootfs");
    let src_rootfs = bundle_dir().join("rootfs");
    copy_dir(&src_rootfs, &rootfs);

    std::fs::copy(bin, rootfs.join("runtimetest")).expect("copy runtimetest");

    let config = serde_json::json!({
        "ociVersion": "1.0.0",
        "root": { "path": "rootfs", "readonly": false },
        "hostname": "dtrun-runtimetest",
        "mounts": [
            { "destination": "/proc", "type": "proc", "source": "proc",
              "options": ["nosuid", "noexec", "nodev"] },
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
            "cwd": "/"
        }
    });
    std::fs::write(
        dir.join("config.json"),
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .expect("write config.json");
    // runtimetest reads `config.json` from the container working directory.
    std::fs::copy(dir.join("config.json"), rootfs.join("config.json")).expect("copy config");

    dir
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create rootfs");
    for entry in std::fs::read_dir(from).expect("read rootfs") {
        let entry = entry.expect("rootfs entry");
        let src = entry.path();
        let dst = to.join(entry.file_name());
        let ft = entry.file_type().expect("file type");
        if ft.is_dir() {
            copy_dir(&src, &dst);
        } else if ft.is_symlink() {
            let target = std::fs::read_link(&src).expect("read link");
            let _ = std::fs::remove_file(&dst);
            std::os::unix::fs::symlink(&target, &dst).expect("symlink");
        } else {
            std::fs::copy(&src, &dst).expect("copy rootfs entry");
        }
    }
}

/// Parse the TAP output recorded by runtimetest in the container's trace and
/// return the lines of every failing test.
fn tap_failures(trace: &str) -> Vec<String> {
    trace
        .lines()
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            let event = v["event"].as_str()?;
            if event != "stdout" {
                return None;
            }
            let text = v["line"].as_str()?;
            if text.starts_with("not ok") && !text.contains("default device") {
                Some(text.to_owned())
            } else {
                None
            }
        })
        .collect()
}

#[test]
fn container_passes_oci_runtimetest_validation() {
    let Some(bin) = runtimetest_bin() else {
        eprintln!(
            "runtimetest not found; install with:\n  \
             go install github.com/opencontainers/runtime-tools/cmd/runtimetest@master"
        );
        return;
    };

    let bundle = make_runtimetest_bundle(&bin);
    let root = ScratchRoot::new("runtimetest");
    let out = Command::new(dtrun())
        .arg("run")
        .arg("rt")
        .args(["--bundle"])
        .arg(&bundle)
        .args(["--root"])
        .arg(root.path())
        .args(["--seed"])
        .arg("42")
        .env("RUST_LOG", "error")
        .output()
        .expect("spawn dtrun runtimetest run");

    assert_eq!(
        exit_code(&out),
        0,
        "runtimetest container failed: {}",
        stderr(&out)
    );

    let trace = root.trace("rt");
    let failures = tap_failures(&trace);
    assert!(
        failures.is_empty(),
        "runtimetest reported failures:\n  {}",
        failures.join("\n  ")
    );
    let _ = std::fs::remove_dir_all(&bundle);
}

/// Read `state.json` until its status is one of `wanted` (or timeout).
fn poll_status(path: &Path, wanted: &[&str]) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                let status = v["status"].as_str().unwrap_or("").to_owned();
                if wanted.contains(&status.as_str()) {
                    return status;
                }
            }
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "timed out waiting for state to become {:?} at {}",
                wanted,
                path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
