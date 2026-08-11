//! Container lifecycle management operations (start, kill, delete, state, list).

use crate::runtime::host::HostError;
use crate::runtime::signal::parse_signal;
use crate::runtime::state::{self, Status};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use std::io::Write;
use std::path::Path;
use tracing::info;

/// `dtrun start`: release a created container by writing the exec FIFO.
pub fn start_container(state_root: &Path, id: &str) -> Result<(), HostError> {
    let st = state::read_state(state_root, id)?;
    if st.status != Status::Created {
        return Err(HostError::NotCreated);
    }
    let fifo = state::exec_fifo(state_root, id);
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fifo)
        .map_err(HostError::Io)?;
    f.write_all(b"x").map_err(HostError::Io)?;
    Ok(())
}

/// `dtrun kill`: send a signal to the container's init process.
pub fn kill_container(state_root: &Path, id: &str, sig: &str) -> Result<(), HostError> {
    let st = state::read_state(state_root, id)?;
    if st.pid <= 0 {
        return Err(HostError::NotFound);
    }
    let signal = parse_signal(sig).map_err(|e| HostError::BadSignal(e.to_string()))?;
    kill(Pid::from_raw(st.pid), signal).map_err(|e| HostError::Errno("kill", e))?;
    Ok(())
}

/// `dtrun delete`: remove container state, optionally killing a live container.
pub fn delete_container(state_root: &Path, id: &str, force: bool) -> Result<(), HostError> {
    let st = state::read_state(state_root, id)?;
    if st.status != Status::Stopped && !force {
        return Err(HostError::Config(format!(
            "container '{}' is still running (use --force)",
            id
        )));
    }
    if force && st.pid > 0 {
        let _ = kill(Pid::from_raw(st.pid), Signal::SIGKILL);
    }
    state::remove_state(state_root, id)?;
    Ok(())
}

/// `dtrun state`: print the container state as JSON.
pub fn print_state(state_root: &Path, id: &str) -> Result<(), HostError> {
    let st = state::read_state(state_root, id)?;
    let json = serde_json::to_string_pretty(&st).map_err(|e| HostError::Config(e.to_string()))?;
    info!(id, state = %json, "state");
    Ok(())
}

/// `dtrun list`: list containers known to the runtime.
pub fn list_containers(state_root: &Path, format: &str) -> Result<(), HostError> {
    let ids = state::list_ids(state_root);
    let rows: Vec<serde_json::Value> = ids
        .iter()
        .filter_map(|id| state::read_state(state_root, id).ok())
        .map(|st| {
            serde_json::json!({
                "id": st.id,
                "pid": st.pid,
                "status": st.status.as_str(),
                "bundle": st.bundle,
                "rootfs": st.rootfs,
                "seed": st.seed,
            })
        })
        .collect();

    match format {
        "json" => {
            let json = serde_json::to_string_pretty(&rows)
                .map_err(|e| HostError::Config(e.to_string()))?;
            info!(containers = %json, "list");
        }
        _ => {
            let mut table = format!("{:<24} {:<9} {:<8} {:<6}\n", "ID", "STATUS", "PID", "SEED");
            for row in &rows {
                table.push_str(&format!(
                    "{:<24} {:<9} {:<8} {:<6}\n",
                    row["id"].as_str().unwrap_or(""),
                    row["status"].as_str().unwrap_or(""),
                    row["pid"].as_i64().unwrap_or(0),
                    row["seed"].as_i64().unwrap_or(0),
                ));
            }
            info!(containers = %table, "list");
        }
    }
    Ok(())
}
