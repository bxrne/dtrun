//! Container runtime: turns an [`OciConfig`] into an isolated, deterministically
//! supervised child, and implements the OCI lifecycle (`create`/`start`/`run`/
//! `kill`/`delete`/`state`/`exec`).

use crate::oci::config::{Mount, OciConfig, Process};
use crate::runtime::mounts::{
    apply_mount, make_root_private, make_root_readonly, setup_default_devices,
};
use crate::runtime::namespaces::map_root_user;
use crate::runtime::state::{self, ContainerState, Status};
use crate::runtime::supervisor;
use crate::runtime::{cgroup, net};
use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::sched::{CloneFlags, clone, setns};
use nix::sys::ptrace::{self, Options};
use nix::sys::signal::{Signal, kill};
use nix::sys::stat::Mode;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::ForkResult;
use nix::unistd::{
    Pid, chdir, chroot, dup2_stderr, dup2_stdout, execvpe, fork, mkfifo, pipe2, read, sethostname,
    write,
};
use std::ffi::CString;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use tracing::{error, info, warn};

/// Stack for the cloned child; sized so PID1 workloads can't overflow it.
const STACK_SIZE: usize = 8 * 1024 * 1024;

/// Namespace flags applied to the cloned child. CLONE_NEWUSER must be set so
/// that an unprivileged caller can create the remaining namespaces, including
/// a dedicated network namespace (CLONE_NEWNET) for isolation.
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

pub struct Host {
    config: OciConfig,
    bundle: PathBuf,
    rootfs: String,
    entrypoint: Option<(String, Vec<String>)>,
    process: Option<Process>,
    mounts: Vec<Mount>,
    env: Vec<String>,
    seed: u64,
}

/// Handles to the container output-relay threads; each returns the captured
/// lines in stream order.
type RelayHandles = Vec<JoinHandle<Vec<String>>>;

impl Host {
    pub fn new(config: OciConfig, bundle: PathBuf, seed: u64) -> Self {
        let entrypoint = config.process.as_ref().and_then(|p| {
            let args = p.args.as_ref()?;
            let (command, rest) = args.split_first()?;
            Some((command.clone(), rest.to_vec()))
        });

        let root_path = Path::new(&config.root.path);
        let rootfs = if root_path.is_absolute() {
            config.root.path.clone()
        } else {
            bundle
                .join(&config.root.path)
                .to_string_lossy()
                .into_owned()
        };

        let env = config
            .process
            .as_ref()
            .and_then(|p| p.env.clone())
            .unwrap_or_default();

        let process = config.process.clone();

        let mounts = config.mounts.clone().unwrap_or_default();

        Self {
            config,
            bundle,
            rootfs,
            entrypoint,
            process,
            mounts,
            env,
            seed,
        }
    }

    /// Validate that the runtime inputs derived from the config are usable.
    pub fn validate(&self) -> Result<(), HostError> {
        if self.entrypoint.is_none() {
            return Err(HostError::Config(
                "no entrypoint: config process.args is missing or empty".into(),
            ));
        }
        let rootfs = Path::new(&self.rootfs);
        if !rootfs.exists() {
            return Err(HostError::Config(format!(
                "rootfs '{}' does not exist",
                self.rootfs
            )));
        }
        if !rootfs.is_dir() {
            return Err(HostError::Config(format!(
                "rootfs '{}' is not a directory",
                self.rootfs
            )));
        }
        Ok(())
    }

    /// `run`: create, start, supervise to completion. Runs in the foreground;
    /// returns the container exit code. The state directory (including the
    /// execution trace) is preserved for `delete`/replay.
    pub fn run(&self, state_root: &Path, id: &str) -> Result<i32, HostError> {
        self.validate()?;
        state::init_container_dir(state_root, id)?;
        state::reset_trace(state_root, id);

        let (child, relays) = self.spawn_created()?;
        let _ = cgroup::apply_limits(child, id, &self.config);

        let mut st = self.mk_state(state_root, id, Status::Running, child, None);
        state::write_state(&st, state_root)?;
        info!(container = id, pid = child.as_raw(), "container running");

        let code = self.supervise_and_finish(state_root, id, child, relays, &mut st)?;
        Ok(code)
    }

    /// `create`: spawn the container into fresh namespaces and pause it at the
    /// syscall boundary. A daemon supervisor owns the container; this returns
    /// the container PID once it reports the `created` state.
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

    /// The daemonised supervisor: spawn the container, report `created`, then
    /// block until `dtrun start` writes the exec FIFO.
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

    /// Wait for `dtrun start`, then supervise the container to completion.
    fn supervisor_wait_and_run(
        &self,
        state_root: &Path,
        id: &str,
        child: Pid,
        relays: RelayHandles,
    ) -> Result<i32, HostError> {
        let fifo = state::exec_fifo(state_root, id);
        // Block until `dtrun start` writes a byte into the FIFO.
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

        let _ = cgroup::apply_limits(child, id, &self.config);

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

    /// Common tail: run the deterministic ptrace loop, then record the final
    /// state and drain any output relays.
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
        let _ = state::write_state(st, state_root);

        // Drain the output relays and append their lines to the trace after
        // the syscall stream, so the trace file is independent of scheduler
        // races between the relay threads and the supervisor.
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

    /// Build a state record for this container.
    fn mk_state(
        &self,
        state_root: &Path,
        id: &str,
        status: Status,
        pid: Pid,
        exit_code: Option<i32>,
    ) -> ContainerState {
        let _ = state_root;
        ContainerState {
            oci_version: self.config.oci_version.clone(),
            id: id.to_owned(),
            status,
            pid: pid.as_raw(),
            bundle: self.bundle.to_string_lossy().into_owned(),
            rootfs: self.rootfs.clone(),
            seed: self.seed,
            exit_code,
            created: state::now_rfc3339(),
        }
    }

    /// Clone a child into fresh namespaces, jail it into the rootfs, pause it
    /// at the first ptrace stop, and arm the supervisor options.
    fn spawn_created(&self) -> Result<(Pid, RelayHandles), HostError> {
        let Some((command, args)) = self.entrypoint.clone() else {
            return Err(HostError::Config("no entrypoint".into()));
        };

        let rootfs = self.rootfs.clone();
        let mounts = self.mounts.clone();
        let hostname = self.config.hostname.clone();
        let cwd = self
            .process
            .as_ref()
            .and_then(|p| p.cwd.clone())
            .unwrap_or_else(|| "/".to_owned());
        let readonly = self.config.root.readonly.unwrap_or(false);
        let env = self.env.clone();

        // Sync pipes: child->parent (child has unshared) and parent->child
        // (id mappings are written). Keeps the id-map writes race-free.
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
            let _ = write(&child_ready_w, b"x");
            let mut ack = [0u8; 1];
            let _ = read(&maps_done_r, &mut ack);

            if let Err(e) = make_root_private() {
                error!("remount / as private failed: {e}");
                return 1;
            }

            if let Some(name) = hostname.clone()
                && let Err(e) = sethostname(name)
            {
                warn!("sethostname failed: {e}");
            }

            // Bring up loopback in the fresh network namespace.
            if let Err(e) = net::bring_up_loopback() {
                warn!("bringing up loopback failed: {e}");
            }

            // Open the host's default device nodes before chroot so they can be
            // bind-mounted into the container's `/dev` (mknod(2) is forbidden
            // inside a user namespace). O_PATH avoids requiring read/write
            // access to each node — `/dev/tty`, for example, can only be
            // opened read/write by a process with a controlling terminal.
            let host_devices: Vec<(String, OwnedFd)> =
                ["null", "zero", "full", "random", "urandom", "tty"]
                    .iter()
                    .filter_map(|name| {
                        let path = format!("/dev/{name}");
                        let c = std::ffi::CString::new(path.as_str()).ok()?;
                        // O_PATH: no read/write permission needed (e.g. /dev/tty
                        // can only be opened read/write by a controlling
                        // terminal). bind_mount resolves the node via
                        // /proc/self/fd, so the fd need never be I/O'd.
                        let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
                        if fd < 0 {
                            None
                        } else {
                            use std::os::fd::FromRawFd;
                            Some((name.to_string(), unsafe { OwnedFd::from_raw_fd(fd) }))
                        }
                    })
                    .collect();

            if let Err(e) = chroot(rootfs.as_str()) {
                error!("chroot({rootfs}) failed: {e}");
                return 1;
            }
            if let Err(e) = chdir(cwd.as_str()) {
                error!("chdir({cwd}) failed: {e}");
                return 1;
            }

            for mount in &mounts {
                if let Err(e) = apply_mount(mount) {
                    warn!(dest = %mount.destination, "mount failed: {e}");
                }
            }

            setup_default_devices(&host_devices);

            if readonly && let Err(e) = make_root_readonly() {
                warn!("readonly root remount failed: {e}");
            }

            // Route the container's stdout/stderr into our capture pipes.
            let _ = dup2_stdout(&stdout_w);
            let _ = dup2_stderr(&stderr_w);

            // Hand control of every syscall to the supervisor, then freeze at
            // the first stop until `dtrun start` (or `run`) releases us.
            if let Err(e) = ptrace::traceme() {
                error!("ptrace TRACEME failed: {e}");
                return 1;
            }
            unsafe { libc::raise(libc::SIGSTOP) };

            exec_entrypoint(&command, &args, &env)
        });

        let child = unsafe {
            clone(
                cb,
                &mut stack,
                NAMESPACE_FLAGS,
                Some(Signal::SIGCHLD as i32),
            )
        }
        .map_err(HostError::Clone)?;

        // The child is now in its own user namespace. Write the id mappings,
        // then let it proceed.
        let mut ready = [0u8; 1];
        let _ = read(&child_ready_r, &mut ready);
        map_root_user(child)
            .map_err(|e| HostError::Config(format!("user namespace setup failed: {e}")))?;
        let _ = write(&maps_done_w, b"x");

        let relays = vec![
            thread::spawn(relay_lines(stdout_r, true)),
            thread::spawn(relay_lines(stderr_r, false)),
        ];

        // Wait for the child's initial ptrace stop (its SIGSTOP).
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

/// Detach from the terminal: new session and stdio redirected to /dev/null so
/// the supervisor outlives `dtrun create`.
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

/// Relay one of the container's output streams into tracing for the live
/// display, returning the captured lines in stream order for the supervisor to
/// append to the trace deterministically.
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
    let signal = parse_signal(sig)?;
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
    let _ = state_root;

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
                        let dot = std::ffi::CString::new(".").expect("no NUL in '.'");
                        libc::chroot(dot.as_ptr());
                        let cwd_c =
                            CString::new(cwd).unwrap_or_else(|_| CString::new("/").unwrap());
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

/// `setns(2)` into one of the container's namespaces via `/proc/<pid>/ns/<ns>`.
fn enter_ns(pid: i32, ns: &str) -> Result<(), HostError> {
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

/// Parse a signal name (e.g. "SIGTERM", "TERM", "9") into a [`Signal`].
fn parse_signal(sig: &str) -> Result<Signal, HostError> {
    let s = sig.trim().to_uppercase();
    let s = s.strip_prefix("SIG").unwrap_or(&s);
    if let Ok(n) = s.parse::<i32>() {
        if let Ok(signal) = Signal::try_from(n) {
            return Ok(signal);
        }
        return Err(HostError::BadSignal(sig.into()));
    }
    let candidates = [
        Signal::SIGHUP,
        Signal::SIGINT,
        Signal::SIGQUIT,
        Signal::SIGILL,
        Signal::SIGTRAP,
        Signal::SIGABRT,
        Signal::SIGBUS,
        Signal::SIGFPE,
        Signal::SIGKILL,
        Signal::SIGUSR1,
        Signal::SIGSEGV,
        Signal::SIGUSR2,
        Signal::SIGPIPE,
        Signal::SIGALRM,
        Signal::SIGTERM,
        Signal::SIGSTKFLT,
        Signal::SIGCHLD,
        Signal::SIGCONT,
        Signal::SIGSTOP,
        Signal::SIGTSTP,
        Signal::SIGTTIN,
        Signal::SIGTTOU,
        Signal::SIGURG,
        Signal::SIGXCPU,
        Signal::SIGXFSZ,
        Signal::SIGVTALRM,
        Signal::SIGPROF,
        Signal::SIGWINCH,
        Signal::SIGIO,
        Signal::SIGPWR,
        Signal::SIGSYS,
    ];
    candidates
        .iter()
        .copied()
        .find(|signal| signal.as_str() == format!("SIG{s}"))
        .ok_or_else(|| HostError::BadSignal(sig.into()))
}

/// Build the C argv for the entrypoint, failing on any NUL byte.
fn build_c_args(command: &str, args: &[String]) -> Option<Vec<CString>> {
    let c_command = CString::new(command).ok()?;
    let mut out = Vec::with_capacity(args.len() + 1);
    out.push(c_command);
    for a in args {
        out.push(CString::new(a.clone()).ok()?);
    }
    Some(out)
}

/// exec the container entrypoint with its args and environment.
/// Only returns on failure.
fn exec_entrypoint(command: &str, args: &[String], env: &[String]) -> isize {
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
