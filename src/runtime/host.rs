//! Container runtime host: turns an [`OciConfig`] into an isolated, deterministically
//! supervised child, and drives the container lifecycle.

use crate::oci::config::{LinuxIdMapping, Mount, OciConfig};
use crate::runtime::cgroup;
use crate::runtime::child::setup_and_exec_child;
use crate::runtime::namespaces::map_root_user;
use crate::runtime::state::{self, ContainerState, Status};
use crate::runtime::supervisor;
use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::sched::{CloneFlags, clone};
use nix::sys::ptrace::{self, Options};
use nix::sys::signal::Signal;
use nix::sys::stat::Mode;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::ForkResult;
use nix::unistd::{Pid, fork, mkfifo, pipe2, read, write};
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use tracing::{info, warn};

pub use crate::runtime::exec::exec_in_container;
pub use crate::runtime::ops::{
    delete_container, kill_container, list_containers, print_state, start_container,
};

/// Stack for the cloned child; sized so PID1 workloads can't overflow it.
const STACK_SIZE: usize = 8 * 1024 * 1024;

/// Namespace flags applied to the cloned child.
const NAMESPACE_FLAGS: CloneFlags = CloneFlags::CLONE_NEWUSER
    .union(CloneFlags::CLONE_NEWPID)
    .union(CloneFlags::CLONE_NEWUTS)
    .union(CloneFlags::CLONE_NEWIPC)
    .union(CloneFlags::CLONE_NEWNS)
    .union(CloneFlags::CLONE_NEWNET);

/// Errors from the runtime host.
#[derive(Debug)]
pub enum HostError {
    Config(String),
    State(state::StateError),
    Io(std::io::Error),
    Errno(&'static str, Errno),
    Clone(Errno),
    Wait(Errno),
    Supervise(supervisor::SuperviseError),
    NotFound,
    NotCreated,
    BadSignal(String),
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::Config(e) => write!(f, "invalid config: {e}"),
            HostError::State(e) => write!(f, "{e}"),
            HostError::Io(e) => write!(f, "io error: {e}"),
            HostError::Errno(op, e) => write!(f, "{op} failed: {e}"),
            HostError::Clone(e) => write!(f, "clone failed: {e}"),
            HostError::Wait(e) => write!(f, "wait failed: {e}"),
            HostError::Supervise(e) => write!(f, "supervisor error: {e}"),
            HostError::NotFound => write!(f, "container not found"),
            HostError::NotCreated => write!(f, "container is not in the created state"),
            HostError::BadSignal(s) => write!(f, "unknown signal '{s}'"),
        }
    }
}

impl std::error::Error for HostError {}

impl From<state::StateError> for HostError {
    fn from(value: state::StateError) -> Self {
        HostError::State(value)
    }
}

impl From<std::io::Error> for HostError {
    fn from(value: std::io::Error) -> Self {
        HostError::Io(value)
    }
}

impl From<supervisor::SuperviseError> for HostError {
    fn from(value: supervisor::SuperviseError) -> Self {
        HostError::Supervise(value)
    }
}

/// Container network mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetMode {
    /// Private network namespace with only loopback (the default).
    None,
    /// Share the host network namespace.
    Host,
}

impl NetMode {
    pub fn parse(value: &str) -> Result<Self, HostError> {
        match value {
            "none" => Ok(NetMode::None),
            "host" => Ok(NetMode::Host),
            other => Err(HostError::Config(format!(
                "unknown network mode '{other}' (expected 'none' or 'host')"
            ))),
        }
    }
}

/// Handles to the container output-relay threads.
type RelayHandles = Vec<JoinHandle<Vec<String>>>;

pub struct Host {
    config: OciConfig,
    bundle: PathBuf,
    seed: u64,
    net_mode: NetMode,
}

impl Host {
    pub fn new(config: OciConfig, bundle: PathBuf, seed: u64) -> Self {
        Self::with_net(config, bundle, seed, NetMode::None)
    }

    pub fn with_net(config: OciConfig, bundle: PathBuf, seed: u64, net_mode: NetMode) -> Self {
        Self {
            config,
            bundle,
            seed,
            net_mode,
        }
    }

    pub fn config(&self) -> &OciConfig {
        &self.config
    }

    pub fn bundle(&self) -> &Path {
        &self.bundle
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    pub fn net_mode(&self) -> NetMode {
        self.net_mode
    }

    pub fn rootfs(&self) -> PathBuf {
        let root_path = Path::new(&self.config.root.path);
        if root_path.is_absolute() {
            root_path.to_path_buf()
        } else {
            self.bundle.join(&self.config.root.path)
        }
    }

    pub fn entrypoint(&self) -> Option<(&str, &[String])> {
        let process = self.config.process.as_ref()?;
        let args = process.args.as_ref()?;
        let (command, rest) = args.split_first()?;
        Some((command.as_str(), rest))
    }

    pub fn env(&self) -> &[String] {
        self.config
            .process
            .as_ref()
            .and_then(|p| p.env.as_deref())
            .unwrap_or(&[])
    }

    pub fn mounts(&self) -> &[Mount] {
        let empty: &[Mount] = &[];
        self.config.mounts.as_deref().unwrap_or(empty)
    }

    /// Validate that the runtime inputs derived from the config are usable.
    pub fn validate(&self) -> Result<(), HostError> {
        if self.entrypoint().is_none() {
            return Err(HostError::Config(
                "no entrypoint: config process.args is missing or empty".into(),
            ));
        }
        let rootfs = self.rootfs();
        if !rootfs.exists() {
            return Err(HostError::Config(format!(
                "rootfs '{}' does not exist",
                rootfs.display()
            )));
        }
        if !rootfs.is_dir() {
            return Err(HostError::Config(format!(
                "rootfs '{}' is not a directory",
                rootfs.display()
            )));
        }
        Ok(())
    }

    /// `run`: create, start, supervise to completion.
    pub fn run(&self, state_root: &Path, id: &str) -> Result<i32, HostError> {
        self.validate()?;
        state::init_container_dir(state_root, id)?;
        state::reset_trace(state_root, id);

        let (child, relays) = self.spawn_created()?;
        if let Err(e) = cgroup::apply_limits(child, id, &self.config) {
            warn!(
                ?e,
                container = id,
                "cgroup limits could not be fully applied"
            );
        }

        let mut st = self.mk_state(state_root, id, Status::Running, child, None);
        state::write_state(&st, state_root)?;
        info!(container = id, pid = child.as_raw(), "container running");

        let code = self.supervise_and_finish(state_root, id, child, relays, &mut st)?;
        Ok(code)
    }

    /// `create`: spawn the container into fresh namespaces and pause it.
    pub fn create(&self, state_root: &Path, id: &str) -> Result<Pid, HostError> {
        self.validate()?;
        state::init_container_dir(state_root, id)?;
        state::reset_trace(state_root, id);
        let fifo = state::exec_fifo(state_root, id);
        match mkfifo(&fifo, Mode::S_IRWXU) {
            Ok(()) => {}
            Err(Errno::EEXIST) => {}
            Err(e) => return Err(HostError::Errno("mkfifo", e)),
        }

        self.create_detached(state_root, id)
    }

    fn create_detached(&self, state_root: &Path, id: &str) -> Result<Pid, HostError> {
        let (ready_r, ready_w) =
            pipe2(OFlag::O_CLOEXEC).map_err(|e| HostError::Errno("pipe2", e))?;

        match unsafe { fork() }.map_err(|e| HostError::Errno("fork", e))? {
            ForkResult::Parent { child } => {
                drop(ready_w);
                let mut line = String::new();
                {
                    let mut reader = BufReader::new(File::from(ready_r));
                    let _ = reader.read_line(&mut line);
                }
                let line = line.trim();
                if line.is_empty() {
                    let _ = waitpid(child, None);
                    return Err(HostError::Config("supervisor failed during create".into()));
                }
                line.parse::<i32>()
                    .map(Pid::from_raw)
                    .map_err(|_| HostError::Config(format!("supervisor error: {line}")))
            }
            ForkResult::Child => {
                drop(ready_r);
                daemonize();
                match self.supervisor_created(state_root, id, &ready_w) {
                    Ok((pid, relays)) => {
                        let _ = write(&ready_w, format!("{}\n", pid.as_raw()).as_bytes());
                        let _ = self.supervisor_wait_and_run(state_root, id, pid, relays);
                    }
                    Err(e) => {
                        let _ = write(&ready_w, format!("{e}\n").as_bytes());
                    }
                }
                unsafe { libc::_exit(0) };
            }
        }
    }

    fn supervisor_created(
        &self,
        state_root: &Path,
        id: &str,
        ready_w: &OwnedFd,
    ) -> Result<(Pid, RelayHandles), HostError> {
        let (child, relays) = self.spawn_created()?;
        let st = self.mk_state(state_root, id, Status::Created, child, None);
        state::write_state(&st, state_root)?;
        info!(container = id, pid = child.as_raw(), "container created");
        let _ = write(ready_w, format!("{}\n", child.as_raw()).as_bytes());
        Ok((child, relays))
    }

    fn supervisor_wait_and_run(
        &self,
        state_root: &Path,
        id: &str,
        child: Pid,
        relays: RelayHandles,
    ) -> Result<i32, HostError> {
        let fifo = state::exec_fifo(state_root, id);
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fifo)
            .map_err(HostError::Io)?;
        let mut byte = [0u8; 1];
        {
            let mut reader = BufReader::new(f);
            let _ = reader.read_exact(&mut byte);
        }

        if let Err(e) = cgroup::apply_limits(child, id, &self.config) {
            warn!(
                ?e,
                container = id,
                "cgroup limits could not be fully applied"
            );
        }

        let mut st = match state::read_state(state_root, id) {
            Ok(s) => s,
            Err(_) => self.mk_state(state_root, id, Status::Created, child, None),
        };
        st.status = Status::Running;
        st.pid = child.as_raw();
        state::write_state(&st, state_root)?;
        info!(container = id, "container started");

        self.supervise_and_finish(state_root, id, child, relays, &mut st)
    }

    fn supervise_and_finish(
        &self,
        state_root: &Path,
        id: &str,
        child: Pid,
        relays: RelayHandles,
        st: &mut ContainerState,
    ) -> Result<i32, HostError> {
        let code = supervisor::supervise(child, self.seed, state_root, id)?;
        st.status = Status::Stopped;
        st.exit_code = Some(code);
        if let Err(e) = state::write_state(st, state_root) {
            warn!(?e, container = id, "failed to update state to stopped");
        }

        let mut captured: Vec<Vec<String>> = Vec::with_capacity(relays.len());
        for handle in relays {
            captured.push(handle.join().unwrap_or_default());
        }
        if let [stdout, stderr] = captured.as_slice() {
            for line in stdout {
                state::trace_event(
                    state_root,
                    id,
                    &serde_json::json!({ "event": "stdout", "line": line }),
                );
            }
            for line in stderr {
                state::trace_event(
                    state_root,
                    id,
                    &serde_json::json!({ "event": "stderr", "line": line }),
                );
            }
        }

        info!(container = id, exit_code = code, "container stopped");
        Ok(code)
    }

    fn mk_state(
        &self,
        _state_root: &Path,
        id: &str,
        status: Status,
        pid: Pid,
        exit_code: Option<i32>,
    ) -> ContainerState {
        ContainerState {
            oci_version: self.config.oci_version.clone(),
            id: id.to_owned(),
            status,
            pid: pid.as_raw(),
            bundle: self.bundle.to_string_lossy().into_owned(),
            rootfs: self.rootfs().to_string_lossy().into_owned(),
            seed: self.seed,
            exit_code,
            created: state::now_rfc3339(),
        }
    }

    fn spawn_created(&self) -> Result<(Pid, RelayHandles), HostError> {
        if self.entrypoint().is_none() {
            return Err(HostError::Config("no entrypoint".into()));
        }

        let rootfs = self.rootfs();
        let net_mode = self.net_mode;
        let config = &self.config;

        let empty_mappings: Vec<LinuxIdMapping> = Vec::new();
        let uid_mappings = config
            .linux
            .as_ref()
            .and_then(|l| l.uid_mappings.as_deref())
            .unwrap_or(&empty_mappings);
        let gid_mappings = config
            .linux
            .as_ref()
            .and_then(|l| l.gid_mappings.as_deref())
            .unwrap_or(&empty_mappings);

        let (child_ready_r, child_ready_w) =
            pipe2(OFlag::O_CLOEXEC).map_err(|e| HostError::Errno("child_ready pipe", e))?;
        let (maps_done_r, maps_done_w) =
            pipe2(OFlag::O_CLOEXEC).map_err(|e| HostError::Errno("maps_done pipe", e))?;
        let (stdout_r, stdout_w) =
            pipe2(OFlag::O_CLOEXEC).map_err(|e| HostError::Errno("stdout pipe", e))?;
        let (stderr_r, stderr_w) =
            pipe2(OFlag::O_CLOEXEC).map_err(|e| HostError::Errno("stderr pipe", e))?;

        let mut stack = vec![0u8; STACK_SIZE];
        let cb: Box<dyn FnMut() -> isize> = Box::new(move || {
            setup_and_exec_child(
                config,
                &rootfs,
                net_mode,
                &child_ready_w,
                &maps_done_r,
                &stdout_w,
                &stderr_w,
            )
        });

        let child = unsafe {
            let mut flags = NAMESPACE_FLAGS;
            if net_mode == NetMode::Host {
                flags &= !CloneFlags::CLONE_NEWNET;
            }
            clone(cb, &mut stack, flags, Some(Signal::SIGCHLD as i32))
        }
        .map_err(HostError::Clone)?;

        let mut ready = [0u8; 1];
        let _ = read(&child_ready_r, &mut ready);
        map_root_user(child, uid_mappings, gid_mappings)
            .map_err(|e| HostError::Config(format!("user namespace setup failed: {e}")))?;
        let _ = write(&maps_done_w, b"x");

        let relays = vec![
            thread::spawn(relay_lines(stdout_r, true)),
            thread::spawn(relay_lines(stderr_r, false)),
        ];

        let status = waitpid(child, Some(WaitPidFlag::__WALL | WaitPidFlag::WUNTRACED))
            .map_err(HostError::Wait)?;
        match status {
            WaitStatus::Stopped(p, Signal::SIGSTOP) => {
                ptrace::setoptions(
                    p,
                    Options::PTRACE_O_TRACESYSGOOD
                        | Options::PTRACE_O_TRACEFORK
                        | Options::PTRACE_O_TRACEVFORK
                        | Options::PTRACE_O_TRACECLONE
                        | Options::PTRACE_O_TRACEEXEC
                        | Options::PTRACE_O_TRACEEXIT
                        | Options::PTRACE_O_EXITKILL,
                )
                .map_err(|e| HostError::Errno("ptrace setoptions", e))?;
                Ok((p, relays))
            }
            WaitStatus::Exited(_p, code) => Err(HostError::Config(format!(
                "container init exited during setup with code {code}"
            ))),
            other => Err(HostError::Config(format!(
                "unexpected container setup result: {other:?}"
            ))),
        }
    }
}

fn daemonize() {
    unsafe {
        libc::setsid();
    }
    if let Ok(null) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
    {
        let fd = std::os::fd::AsRawFd::as_raw_fd(&null);
        unsafe {
            libc::dup2(fd, 0);
            libc::dup2(fd, 1);
            libc::dup2(fd, 2);
        }
    }
}

fn relay_lines(fd: OwnedFd, is_stdout: bool) -> impl FnOnce() -> Vec<String> {
    move || {
        let mut reader = BufReader::new(File::from(fd));
        let mut line = String::new();
        let mut captured = Vec::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim_end().to_owned();
                    if is_stdout {
                        info!(stream = "stdout", line = %trimmed);
                    } else {
                        warn!(stream = "stderr", line = %trimmed);
                    }
                    captured.push(trimmed);
                }
                Err(e) => {
                    warn!(?e, "failed reading container output");
                    break;
                }
            }
        }
        captured
    }
}
