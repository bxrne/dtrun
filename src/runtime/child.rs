//! In-child container initialization routine.
//!
//! Executes inside the cloned child process inside the new namespaces,
//! setting up rootfs, mounts, devices, sysctl, capabilities, and stdio
//! before calling `exec_entrypoint`.

use crate::oci::config::{LinuxDevice, Mount, OciConfig};
use crate::runtime::credentials::{apply_capabilities, apply_oom_score_adj};
use crate::runtime::exec::exec_entrypoint;
use crate::runtime::host::NetMode;
use crate::runtime::mounts::{
    apply_masked_paths, apply_mount, apply_readonly_paths, make_root_private, make_root_readonly,
    setup_default_devices,
};
use crate::runtime::net;
use nix::sys::personality::{self, Persona};
use nix::sys::ptrace;
use nix::unistd::{chdir, chroot, dup2_stderr, dup2_stdout, read, sethostname, write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::Path;
use tracing::{error, warn};

/// Setup container environment in child process and exec entrypoint.
pub fn setup_and_exec_child(
    config: &OciConfig,
    rootfs: &Path,
    net_mode: NetMode,
    child_ready_w: &OwnedFd,
    maps_done_r: &OwnedFd,
    stdout_w: &OwnedFd,
    stderr_w: &OwnedFd,
) -> isize {
    if write(child_ready_w, b"x").is_err() {
        error!("child failed to signal ready to parent");
        return 1;
    }
    let mut ack = [0u8; 1];
    if read(maps_done_r, &mut ack).is_err() {
        error!("child failed to wait for parent id mappings");
        return 1;
    }

    if let Err(e) = make_root_private() {
        error!("remount / as private failed: {e}");
        return 1;
    }

    if let Some(name) = &config.hostname {
        if let Err(e) = sethostname(name) {
            error!("sethostname('{name}') failed: {e}");
            return 1;
        }
    }

    if net_mode == NetMode::None {
        if let Err(e) = net::bring_up_loopback() {
            error!("bringing up loopback failed: {e}");
            return 1;
        }
    }

    let devices: &[LinuxDevice] = config
        .linux
        .as_ref()
        .and_then(|l| l.devices.as_deref())
        .unwrap_or(&[]);

    let mut host_devices: Vec<(String, OwnedFd)> =
        ["null", "zero", "full", "random", "urandom", "tty"]
            .iter()
            .filter_map(|name| {
                let path = format!("/dev/{name}");
                let c = std::ffi::CString::new(path.as_str()).ok()?;
                let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
                if fd < 0 {
                    None
                } else {
                    Some((path, unsafe { OwnedFd::from_raw_fd(fd) }))
                }
            })
            .collect();

    for device in devices {
        if let Ok(c) = std::ffi::CString::new(device.path.as_str()) {
            let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
            if fd >= 0 {
                host_devices.push((device.path.clone(), unsafe { OwnedFd::from_raw_fd(fd) }));
            } else {
                tracing::debug!(path = %device.path, "configured device not found on host");
            }
        }
    }

    let rootfs_str = rootfs.to_string_lossy();
    if let Err(e) = chroot(rootfs_str.as_ref()) {
        error!("chroot({rootfs_str}) failed: {e}");
        return 1;
    }

    let cwd = config
        .process
        .as_ref()
        .and_then(|p| p.cwd.as_deref())
        .unwrap_or("/");
    if let Err(e) = chdir(cwd) {
        error!("chdir({cwd}) failed: {e}");
        return 1;
    }

    let empty_mounts: Vec<Mount> = Vec::new();
    let mounts: &[Mount] = config.mounts.as_deref().unwrap_or(&empty_mounts);
    for mount in mounts {
        if let Err(e) = apply_mount(mount) {
            error!(dest = %mount.destination, "mount failed: {e}");
            return 1;
        }
    }

    setup_default_devices(&host_devices);

    if net_mode != NetMode::Host {
        if let Some(sysctls) = config.linux.as_ref().and_then(|l| l.sysctl.as_ref()) {
            for (key, value) in sysctls {
                let path = format!("/proc/sys/{}", key.replace('.', "/"));
                if let Err(e) = std::fs::write(&path, value) {
                    error!(sysctl = %key, ?e, "sysctl write failed");
                    return 1;
                }
            }
        }
    }

    let empty_paths: Vec<String> = Vec::new();
    let masked_paths = config
        .linux
        .as_ref()
        .and_then(|l| l.masked_paths.as_deref())
        .unwrap_or(&empty_paths);
    let readonly_paths = config
        .linux
        .as_ref()
        .and_then(|l| l.readonly_paths.as_deref())
        .unwrap_or(&empty_paths);

    apply_masked_paths(masked_paths);
    apply_readonly_paths(readonly_paths);

    if let Some(adj) = config.process.as_ref().and_then(|p| p.oom_score_adj) {
        if let Err(e) = apply_oom_score_adj(adj) {
            error!(?e, "oom_score_adj write failed");
            return 1;
        }
    }

    if let Some(caps) = config.process.as_ref().and_then(|p| p.capabilities.as_ref()) {
        if let Err(e) = apply_capabilities(caps) {
            error!(?e, "capability setup failed");
            return 1;
        }
    }

    if config.root.readonly.unwrap_or(false) {
        if let Err(e) = make_root_readonly() {
            error!("readonly root remount failed: {e}");
            return 1;
        }
    }

    if let Err(e) = dup2_stdout(stdout_w) {
        error!("dup2_stdout failed: {e}");
        return 1;
    }
    if let Err(e) = dup2_stderr(stderr_w) {
        error!("dup2_stderr failed: {e}");
        return 1;
    }

    if let Err(e) = ptrace::traceme() {
        error!("ptrace TRACEME failed: {e}");
        return 1;
    }
    unsafe { libc::raise(libc::SIGSTOP) };

    match personality::get() {
        Ok(pers) => {
            let _ = personality::set(pers | Persona::ADDR_NO_RANDOMIZE);
        }
        Err(e) => warn!(?e, "could not disable ASLR"),
    }

    let (command, args) = match config.process.as_ref().and_then(|p| p.args.as_ref()) {
        Some(a) if !a.is_empty() => (&a[0], &a[1..]),
        _ => {
            error!("no entrypoint command specified");
            return 1;
        }
    };

    let empty_env: Vec<String> = Vec::new();
    let env = config
        .process
        .as_ref()
        .and_then(|p| p.env.as_deref())
        .unwrap_or(&empty_env);

    exec_entrypoint(command, args, env)
}
