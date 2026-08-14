//! Container exec and entrypoint execution primitives.

use crate::runtime::host::HostError;
use crate::runtime::state::{self, Status};
use nix::errno::Errno;
use nix::sched::{CloneFlags, setns};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{ForkResult, execvpe, fork};
use std::ffi::CString;
use std::path::Path;
use tracing::error;

/// Build the C argv for the entrypoint, failing on any NUL byte.
pub fn build_c_args(command: &str, args: &[String]) -> Option<Vec<CString>> {
    let c_command = CString::new(command).ok()?;
    let mut out = Vec::with_capacity(args.len() + 1);
    out.push(c_command);
    for a in args {
        out.push(CString::new(a.clone()).ok()?);
    }
    Some(out)
}

/// Exec the container entrypoint with its args and environment.
/// Only returns on failure.
pub fn exec_entrypoint(command: &str, args: &[String], env: &[String]) -> isize {
    let Some(c_args) = build_c_args(command, args) else {
        error!("entrypoint or an argument contains a NUL byte");
        return 1;
    };

    let c_env: Vec<CString> = env
        .iter()
        .filter_map(|kv| CString::new(kv.clone()).ok())
        .collect();

    match execvpe(&c_args[0], &c_args, &c_env) {
        Err(e) => {
            error!("execvpe failed: {e}");
            1
        }
        Ok(_) => 1,
    }
}

/// `setns(2)` into one of the container's namespaces via `/proc/<pid>/ns/<ns>`.
pub fn enter_ns(pid: i32, ns: &str) -> Result<(), HostError> {
    let path = format!("/proc/{pid}/ns/{ns}");
    let f = std::fs::File::open(&path).map_err(HostError::Io)?;
    let flags = match ns {
        "user" => CloneFlags::CLONE_NEWUSER,
        "mnt" => CloneFlags::CLONE_NEWNS,
        "pid" => CloneFlags::CLONE_NEWPID,
        "net" => CloneFlags::CLONE_NEWNET,
        "ipc" => CloneFlags::CLONE_NEWIPC,
        "uts" => CloneFlags::CLONE_NEWUTS,
        _ => CloneFlags::empty(),
    };
    setns(f, flags).map_err(|e| HostError::Errno("setns", e))
}

/// `dtrun exec`: run a command inside the container's namespaces.
pub fn exec_in_container(
    state_root: &Path,
    id: &str,
    command: &[String],
    cwd: Option<&str>,
    env: &[String],
) -> Result<i32, HostError> {
    let st = state::read_state(state_root, id)?;
    if st.pid <= 0 || st.status == Status::Stopped {
        return Err(HostError::Config("container is not running".into()));
    }

    let pid = st.pid;
    let rootfs = st.rootfs.clone();
    let cwd = cwd.unwrap_or("/").to_owned();

    // Open the target rootfs directory before joining the mount namespace,
    // since the host path is not visible from inside the container.
    let rootfd = unsafe {
        let path = match CString::new(rootfs.clone()) {
            Ok(p) => p,
            Err(e) => return Err(HostError::Config(format!("rootfs path: {e}"))),
        };
        libc::open(
            path.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if rootfd < 0 {
        return Err(HostError::Errno("open rootfs", Errno::last()));
    }

    match unsafe { fork() }.map_err(|e| HostError::Errno("fork", e))? {
        ForkResult::Parent { child } => {
            let status = waitpid(child, None).map_err(HostError::Wait)?;
            Ok(match status {
                WaitStatus::Exited(_p, code) => code,
                WaitStatus::Signaled(_p, sig, _) => 128 + sig as i32,
                _ => 1,
            })
        }
        ForkResult::Child => {
            // Join the container's namespaces: user first, then the rest.
            enter_ns(pid, "user")?;
            enter_ns(pid, "mnt")?;
            enter_ns(pid, "net")?;
            enter_ns(pid, "ipc")?;
            enter_ns(pid, "uts")?;
            enter_ns(pid, "pid")?;

            // Entering a PID namespace takes effect on the next fork.
            match unsafe { fork() }.map_err(|e| HostError::Errno("fork", e))? {
                ForkResult::Child => {
                    unsafe {
                        libc::fchdir(rootfd);
                        libc::chroot(c".".as_ptr());
                        let cwd_c = CString::new(cwd)
                            .unwrap_or_else(|_| CString::from_vec_unchecked(b"/".to_vec()));
                        libc::chdir(cwd_c.as_ptr());
                    }

                    let mut c_env: Vec<CString> = std::env::vars()
                        .filter_map(|(k, v)| CString::new(format!("{k}={v}")).ok())
                        .collect();
                    for kv in env {
                        if let Ok(c) = CString::new(kv.clone()) {
                            c_env.push(c);
                        }
                    }

                    let cargs: Vec<CString> = command
                        .iter()
                        .filter_map(|a| CString::new(a.clone()).ok())
                        .collect();
                    if cargs.is_empty() {
                        unsafe { libc::_exit(127) };
                    }

                    match execvpe(&cargs[0], &cargs, &c_env) {
                        Ok(never) => match never {},
                        Err(e) => {
                            error!("exec failed: {e}");
                            unsafe { libc::_exit(127) }
                        }
                    }
                }
                ForkResult::Parent { child } => {
                    let status =
                        waitpid(child, Some(WaitPidFlag::__WALL)).map_err(HostError::Wait)?;
                    let code = match status {
                        WaitStatus::Exited(_p, code) => code,
                        WaitStatus::Signaled(_p, sig, _) => 128 + sig as i32,
                        _ => 1,
                    };
                    unsafe { libc::_exit(code) }
                }
            }
        }
    }
}
