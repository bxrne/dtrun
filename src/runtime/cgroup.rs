//! Best-effort cgroup v2 resource limits.
//!
//! Deterministic execution needs bounded, reproducible resource availability.
//! When the host delegates a writable cgroup v2 tree to the caller, dtrun
//! places the container in a dedicated cgroup and pins CPU/memory/PID limits
//! derived from the OCI `linux.resources` block. Everything here is
//! best-effort: on hosts without delegation it degrades to no limits.

use crate::oci::config::OciConfig;
use nix::unistd::Pid;
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::warn;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Errors from cgroup setup.
#[derive(Debug)]
pub enum CgroupError {
    Io(std::io::Error),
    NotV2,
}

impl std::fmt::Display for CgroupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CgroupError::Io(e) => write!(f, "cgroup io error: {e}"),
            CgroupError::NotV2 => write!(f, "cgroup v2 is not available"),
        }
    }
}

impl std::error::Error for CgroupError {}

/// Create a dedicated cgroup for the container and write the configured
/// resource limits. On any failure a warning is logged and `Ok` is returned,
/// so containers never fail to start because cgroup delegation is missing.
pub fn apply_limits(pid: Pid, id: &str, config: &OciConfig) -> Result<(), CgroupError> {
    let res = match config.linux.as_ref().and_then(|l| l.resources.as_ref()) {
        Some(r) => r,
        None => return Ok(()),
    };

    if !Path::new(CGROUP_ROOT).join("cgroup.controllers").exists() {
        return Err(CgroupError::NotV2);
    }

    let dir = match setup_cgroup(id) {
        Ok(d) => d,
        Err(e) => {
            warn!(?e, "cgroup delegation unavailable; running without limits");
            return Ok(());
        }
    };

    let cpu = res.extra.get("cpu").cloned().unwrap_or(Value::Null);
    let mem = res.extra.get("memory").cloned().unwrap_or(Value::Null);
    let pids = res.extra.get("pids").cloned().unwrap_or(Value::Null);

    write_limit(
        &dir,
        "pids.max",
        pids.get("limit")
            .and_then(Value::as_u64)
            .map(|v| v.to_string()),
    );

    write_limit(
        &dir,
        "memory.max",
        mem.get("limit")
            .and_then(Value::as_u64)
            .map(|v| v.to_string()),
    );
    write_limit(
        &dir,
        "memory.swap.max",
        mem.get("swap")
            .and_then(Value::as_u64)
            .map(|v| v.to_string()),
    );

    let period = cpu.get("period").and_then(Value::as_u64);
    let quota = cpu.get("quota").and_then(Value::as_i64);
    if let (Some(p), Some(q)) = (period, quota) {
        // quota == -1 means no limit.
        write_limit(&dir, "cpu.max", Some(format!("{q} {p}")));
    }
    let shares = cpu.get("shares").and_then(Value::as_u64);
    if let Some(shares) = shares {
        // Convert OCI cpu.shares (1024 base) to cgroup v2 cpu.weight (100 base).
        let weight = 1 + shares.saturating_sub(2) * 9999 / 262_142;
        write_limit(&dir, "cpu.weight", Some(weight.to_string()));
    }

    // Move the container init into the cgroup.
    if let Err(e) = write(&dir.join("cgroup.procs"), pid.as_raw().to_string()) {
        warn!(?e, "cannot move pid {} into cgroup", pid.as_raw());
    }

    Ok(())
}

/// Create (or reuse) `/sys/fs/cgroup/dtrun/<id>`.
fn setup_cgroup(id: &str) -> std::io::Result<PathBuf> {
    let parent = Path::new(CGROUP_ROOT).join("dtrun");
    fs::create_dir_all(&parent)?;
    let dir = parent.join(sanitize_id(id));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Replace characters unsafe for a cgroup directory name.
fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn write_limit(dir: &Path, file: &str, value: Option<String>) {
    let Some(value) = value else { return };
    if let Err(e) = write(&dir.join(file), value) {
        warn!(file, %e, "failed to set cgroup limit");
    }
}

fn write(path: &Path, value: String) -> std::io::Result<()> {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    f.write_all(value.as_bytes())?;
    Ok(())
}
