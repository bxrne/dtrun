//! OCI container state persistence: lifecycle status, state JSON, and the
//! per-container exec FIFO used to separate `create` from `start`.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};

/// Lifecycle status of a container, per the OCI runtime state model.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Creating,
    Created,
    Running,
    Stopped,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Creating => "creating",
            Status::Created => "created",
            Status::Running => "running",
            Status::Stopped => "stopped",
        }
    }
}

/// The OCI `state` object plus dtrun-specific bookkeeping.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ContainerState {
    /// The `ociVersion` from the bundle's `config.json`.
    pub oci_version: String,
    /// The container ID.
    pub id: String,
    /// The runtime status.
    pub status: Status,
    /// PID of the container's init process (host namespace).
    pub pid: i32,
    /// Absolute path to the bundle directory.
    pub bundle: String,
    /// Absolute path to the container's root filesystem.
    pub rootfs: String,
    /// Determinism seed used for this container.
    pub seed: u64,
    /// Exit code once the container has stopped.
    pub exit_code: Option<i32>,
    /// Creation timestamp (RFC 3339).
    pub created: String,
}

/// Errors while reading or writing container state.
#[derive(Debug)]
pub enum StateError {
    Io(std::io::Error),
    Parse(serde_json::Error),
    Missing,
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StateError::Io(e) => write!(f, "state io error: {e}"),
            StateError::Parse(e) => write!(f, "state parse error: {e}"),
            StateError::Missing => write!(f, "container not found"),
        }
    }
}

impl std::error::Error for StateError {}

impl From<std::io::Error> for StateError {
    fn from(value: std::io::Error) -> Self {
        StateError::Io(value)
    }
}

impl From<serde_json::Error> for StateError {
    fn from(value: serde_json::Error) -> Self {
        StateError::Parse(value)
    }
}

/// Directory containing all state for one container.
pub fn container_dir(root: &Path, id: &str) -> PathBuf {
    root.join(id)
}

pub fn state_file(root: &Path, id: &str) -> PathBuf {
    container_dir(root, id).join("state.json")
}

pub fn exec_fifo(root: &Path, id: &str) -> PathBuf {
    container_dir(root, id).join("exec.fifo")
}

pub fn trace_file(root: &Path, id: &str) -> PathBuf {
    container_dir(root, id).join("trace.jsonl")
}

pub fn now_rfc3339() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    chrono_like(now)
}

/// Minimal RFC 3339 formatter to avoid pulling in a chrono dependency.
fn chrono_like(secs: u64) -> String {
    // 2026-08-10T00:00:00Z placeholder backed by libc localtime_r.
    let ts: libc::time_t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&ts as *const libc::time_t as *mut libc::time_t, &mut tm);
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// Create the per-container state directory.
pub fn init_container_dir(root: &Path, id: &str) -> std::io::Result<PathBuf> {
    let dir = container_dir(root, id);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Persist the container state as `state.json`.
pub fn write_state(state: &ContainerState, root: &Path) -> Result<(), StateError> {
    let path = state_file(root, &state.id);
    let json = serde_json::to_string_pretty(state)?;
    let mut f = fs::File::create(path)?;
    f.write_all(json.as_bytes())?;
    Ok(())
}

/// Read the container state, or `StateError::Missing` if it does not exist.
pub fn read_state(root: &Path, id: &str) -> Result<ContainerState, StateError> {
    let path = state_file(root, id);
    let content = fs::read_to_string(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => StateError::Missing,
        _ => StateError::Io(e),
    })?;
    serde_json::from_str(&content).map_err(StateError::Parse)
}

/// Remove the container's state directory.
pub fn remove_state(root: &Path, id: &str) -> Result<(), StateError> {
    let dir = container_dir(root, id);
    fs::remove_dir_all(&dir).map_err(StateError::Io)?;
    Ok(())
}

/// List container IDs known to this runtime root.
pub fn list_ids(root: &Path) -> Vec<String> {
    let mut ids: Vec<String> = match fs::read_dir(root) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().join("state.json").exists())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect(),
        Err(_) => Vec::new(),
    };
    ids.sort();
    ids
}

/// Truncate (or create) the per-container trace file at the start of a run.
pub fn reset_trace(root: &Path, id: &str) {
    let path = trace_file(root, id);
    if let Err(e) = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path)
    {
        warn!(?e, path = %path.display(), "cannot reset trace file");
    }
}

/// Write an event line to the per-container trace file, if one exists.
pub fn trace_event(root: &Path, id: &str, event: &serde_json::Value) {
    let path = trace_file(root, id);
    let mut f = match fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            warn!(?e, "cannot open trace file");
            return;
        }
    };
    if let Ok(line) = serde_json::to_string(event)
        && let Err(e) = writeln!(f, "{line}")
    {
        debug!(?e, "trace write failed");
    }
}
