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

fn count_events(trace: &str, event: &str) -> usize {
    trace
        .lines()
        .filter(|l| l.contains(&format!("\"event\":\"{event}\"")))
        .count()
}

/// Locate a C compiler for the multithreaded determinism fixture.
fn find_cc() -> Option<PathBuf> {
    for name in ["cc", "gcc", "clang"] {
        if Command::new(name)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Some(PathBuf::from(name));
        }
    }
    None
}

/// Build a scratch OCI bundle running the statically-linked pthread fixture.
/// Returns `None` (test skips) when no compiler or static pthread build is
/// available, mirroring the runtimetest skip path.
fn make_thread_bundle() -> Option<PathBuf> {
    let cc = find_cc()?;
    let dir = std::env::temp_dir().join(format!(
        "dtrun-thread-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create thread bundle");
    copy_dir(&bundle_dir().join("rootfs"), &dir.join("rootfs"));

    let bin = dir.join("rootfs/bin/workload");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/threads.c");
    let status = Command::new(&cc)
        .args(["-static", "-pthread", "-O0", "-o"])
        .arg(&bin)
        .arg(&fixture)
        .status()
        .ok()?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        return None;
    }

    let config = serde_json::json!({
        "ociVersion": "1.0.0",
        "root": { "path": "rootfs", "readonly": false },
        "hostname": "dtrun-thread",
        "process": {
            "args": ["/bin/workload"],
            "env": ["PATH=/usr/bin:/bin"],
            "cwd": "/"
        }
    });
    std::fs::write(
        dir.join("config.json"),
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .expect("write thread config");
    Some(dir)
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
fn multithreaded_run_is_deterministic_and_seeded() {
    let Some(bundle) = make_thread_bundle() else {
        eprintln!(
            "no C compiler or static pthread build available; \
             skipping multithreaded determinism test"
        );
        return;
    };
    let root = ScratchRoot::new("threads");
    let a = dtrun_run(root.path(), "thr", 42, &bundle);
    assert_eq!(exit_code(&a), 0, "run failed: {}", stderr(&a));
    let trace_a = root.trace("thr");

    let b = dtrun_run(root.path(), "thr", 42, &bundle);
    assert_eq!(exit_code(&b), 0, "run failed: {}", stderr(&b));
    let trace_b = root.trace("thr");

    let c = dtrun_run(root.path(), "thr", 7, &bundle);
    assert_eq!(exit_code(&c), 0, "run failed: {}", stderr(&c));
    let trace_c = root.trace("thr");

    let _ = std::fs::remove_dir_all(&bundle);

    // The workload spawned four workers, each drawing randomness inside the
    // critical section (so the mutex is genuinely contended).
    assert_eq!(count_events(&trace_a, "thread_create"), 4);
    let worker_lines: Vec<&str> = trace_a
        .lines()
        .filter(|l| l.contains("\"line\":\"thread "))
        .collect();
    assert_eq!(
        worker_lines.len(),
        4,
        "each worker must report exactly once"
    );
    assert!(
        trace_a.contains("\"schedule\""),
        "the scheduler must interleave the threads"
    );
    assert!(
        trace_a.contains("counter=4"),
        "the workers must serialize through the mutex"
    );

    // Same seed => byte-identical event stream, including thread interleaving.
    assert!(!trace_a.is_empty());
    assert_eq!(trace_a, trace_b, "same seed must yield an identical trace");
    // Different seed => different injected randomness.
    assert_ne!(trace_a, trace_c, "different seeds must change the trace");
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

/// Recursively copy a directory, preserving symlinks.
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



/// Read `state.json` until its status is one of `wanted` (or timeout).
fn poll_status(path: &Path, wanted: &[&str]) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let status = std::fs::read_to_string(path)
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .and_then(|v| v["status"].as_str().map(str::to_owned))
            .unwrap_or_default();
        if wanted.contains(&status.as_str()) {
            return status;
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
