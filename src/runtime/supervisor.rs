//! Deterministic supervisor: drives the container under `PTRACE_SYSCALL`,
//! intercepting the sources of nondeterminism the kernel exposes to the
//! workload: randomness (`getrandom`), wall/monotonic time (`clock_gettime`,
//! `gettimeofday`), and, for multi-threaded workloads, the scheduling of
//! threads and the futexes they synchronise on.
//!
//! Threads are run one at a time in FIFO round-robin order and are switched
//! only at syscall boundaries, so the interleaving is fully reproducible. A
//! `futex` waiter is simulated: instead of letting it block in the kernel it
//! is parked and another thread runs, so a waiting thread can never stall the
//! single-runner scheduler. Every scheduling decision and injection is recorded
//! in the execution trace for later replay.

use crate::runtime::state;
use nix::errno::Errno;
use nix::sys::ptrace;
use nix::sys::signal::Signal;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use serde_json::json;
use std::collections::{HashMap, HashSet, VecDeque};
use std::os::unix::fs::FileExt;
use std::path::Path;

/// `PTRACE_GET_SYSCALL_INFO` op codes (linux `uapi/linux/ptrace.h`).
const PTRACE_SYSCALL_INFO_ENTRY: u8 = 1;
const PTRACE_SYSCALL_INFO_EXIT: u8 = 2;

/// Fixed virtual epoch (2024-01-01T00:00:00Z) plus a deterministic seed offset.
const VIRTUAL_EPOCH: u64 = 1_704_067_200;

/// Monotonic clock step: each deterministic time read advances by 1ms.
const MONO_STEP_NS: u64 = 1_000_000;

const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;
const CLOCK_REALTIME_COARSE: i32 = 5;
const CLOCK_MONOTONIC_COARSE: i32 = 6;
const CLOCK_BOOTTIME: i32 = 7;

/// `clock_gettime64` (403) exists in the kernel ABI even on 64-bit targets,
/// but glibc only exposes `SYS_clock_gettime` on x86_64.
const SYS_CLOCK_GETTIME64: i64 = 403;

/// Mask stripping the `FUTEX_PRIVATE_FLAG`/`FUTEX_CLOCK_REALTIME` flag bits
/// from a futex op to reveal the underlying command.
const FUTEX_CMD_MASK: i32 = 0x7f;

/// Deterministic PRNG (xorshift64*), seeded from the CLI `--seed`.
struct Prng(u64);

impl Prng {
    fn new(seed: u64) -> Self {
        Prng(seed.max(1))
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn fill(&mut self, out: &mut [u8]) {
        for chunk in out.chunks_mut(8) {
            let bytes = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
}

/// A syscall whose real kernel behaviour has been replaced.
struct PendingInject {
    syscall: i64,
    retval: i64,
}

/// The interesting arguments of a `clone`/`clone3` call, recorded at syscall
/// entry and consumed when the `PTRACE_EVENT_CLONE` stop reports the new pid.
#[derive(Clone, Copy, Default)]
struct CloneInfo {
    flags: u64,
    child_tid: u64,
}

/// Bookkeeping for a traced thread (or separate process created by fork).
struct Thread {
    /// Stable per-run identifier (0 for the init), used in the trace so the
    /// recorded event stream is independent of host-assigned pid numbers.
    vtid: u64,
    /// The `CLONE_CHILD_*` destination the kernel clears when this thread
    /// exits, used to wake simulated futex waiters (pthread_join).
    child_tid: u64,
    flags: u64,
    /// Whether this task shares the init's address space and participates in
    /// the deterministic single-runner scheduler. Fork/vfork children (separate
    /// address spaces) are excluded: they communicate via blocking pipes and
    /// must be allowed to run concurrently to make progress.
    scheduled: bool,
}

/// Lowercase hex encoding for recording injected bytes in the trace.
fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

/// Errors from the deterministic supervisor.
#[derive(Debug)]
pub enum SuperviseError {
    Ptrace(Errno),
    Wait(Errno),
    Io(std::io::Error),
    /// Every thread is parked on a futex wait and nothing remains to wake it.
    Deadlock,
}

impl std::fmt::Display for SuperviseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SuperviseError::Ptrace(e) => write!(f, "ptrace failed: {e}"),
            SuperviseError::Wait(e) => write!(f, "waitpid failed: {e}"),
            SuperviseError::Io(e) => write!(f, "io failed: {e}"),
            SuperviseError::Deadlock => {
                write!(f, "all threads are parked on futex waits (deadlock)")
            }
        }
    }
}

impl std::error::Error for SuperviseError {}

/// Run the deterministic ptrace loop for the container until its init exits.
/// Returns the container's exit code.
///
/// `state_root`/`id` are used to append to the execution event trace.
pub fn supervise(pid: Pid, seed: u64, state_root: &Path, id: &str) -> Result<i32, SuperviseError> {
    let mut sv = Supervisor {
        pid,
        rng: Prng::new(seed),
        tick: 0,
        pending: HashMap::new(),
        seq: 0,
        state_root: state_root.to_path_buf(),
        id: id.to_owned(),
        threads: HashMap::from([(
            pid,
            Thread {
                vtid: 0,
                child_tid: 0,
                flags: 0,
                scheduled: true,
            },
        )]),
        vtid_next: 1,
        runnable: VecDeque::from([pid]),
        pending_start: HashSet::new(),
        stopped_pending: HashSet::new(),
        blocked: HashSet::new(),
        waiting: HashMap::new(),
        current: None,
        last_vtid: None,
        sig_pending: HashMap::new(),
        clone_pending: HashMap::new(),
        group_exiting: false,
    };
    sv.run()
}

struct Supervisor {
    /// The container init (the thread the supervisor was handed at start).
    pid: Pid,
    rng: Prng,
    tick: u64,
    pending: HashMap<Pid, PendingInject>,
    seq: u64,
    state_root: std::path::PathBuf,
    id: String,

    /// All live traced threads: `pid -> Thread`.
    threads: HashMap<Pid, Thread>,
    /// Virtual thread id issued to the next registered thread.
    vtid_next: u64,
    /// Threads ready to run, in FIFO order.
    runnable: VecDeque<Pid>,
    /// Registered threads whose attach SIGSTOP stop has not been observed yet.
    /// They are only released (scheduled or resumed) once that stop arrives,
    /// otherwise `PTRACE_SYSCALL` can race a still-running new child (ESRCH).
    pending_start: HashSet<Pid>,
    /// Unregistered pids whose attach SIGSTOP stop was observed. They are
    /// parked here until their clone event registers them.
    stopped_pending: HashSet<Pid>,
    /// Threads parked on a simulated `futex` wait.
    blocked: HashSet<Pid>,
    /// Parked threads grouped by the futex word they wait on.
    waiting: HashMap<u64, VecDeque<Pid>>,
    /// The thread currently running (the only one resumed by the tracer).
    current: Option<Pid>,
    /// Vtid of the most recently scheduled thread (for `schedule` events).
    last_vtid: Option<u64>,
    /// Signals received while a thread was not running, delivered on resume.
    sig_pending: HashMap<Pid, Signal>,
    /// `clone`/`clone3` arguments awaiting their `PTRACE_EVENT_CLONE` stop.
    clone_pending: HashMap<Pid, CloneInfo>,
    /// Set once `exit_group` is in flight: every thread is about to die, so the
    /// scheduler must not resume anything (threads can be reaped before their
    /// exit events are processed, making `ptrace` return ESRCH).
    group_exiting: bool,
}

impl Supervisor {
    fn run(&mut self) -> Result<i32, SuperviseError> {
        // Resume the init, consuming its initial SIGSTOP pause; only this
        // thread runs until the next ptrace stop.
        self.schedule_next()?;

        loop {
            let status = self.wait()?;
            if let Some(code) = self.dispatch(status)? {
                return Ok(code);
            }
            if self.current.is_none() {
                self.schedule_next()?;
            }
        }
    }

    fn wait(&mut self) -> Result<WaitStatus, SuperviseError> {
        waitpid(
            Pid::from_raw(-1),
            Some(WaitPidFlag::__WALL | WaitPidFlag::WUNTRACED),
        )
        .map_err(SuperviseError::Wait)
    }

    /// Handle one waitpid status. Returns `Some(code)` when the init has exited
    /// (code is its exit status) and the run is over.
    fn dispatch(&mut self, status: WaitStatus) -> Result<Option<i32>, SuperviseError> {
        match status {
            WaitStatus::PtraceSyscall(p) => {
                let info = ptrace::syscall_info(p).map_err(SuperviseError::Ptrace)?;
                if !self.is_scheduled(p) {
                    // A fork/vfork child (separate address space): handle
                    // its syscalls and let it keep running concurrently.
                    match info.op {
                        PTRACE_SYSCALL_INFO_ENTRY => {
                            self.on_syscall_entry(p)?;
                        }
                        PTRACE_SYSCALL_INFO_EXIT => {
                            self.on_syscall_exit(p)?;
                        }
                        _ => {}
                    }
                    self.resume_thread(p)?;
                    return Ok(None);
                }
                match info.op {
                    PTRACE_SYSCALL_INFO_ENTRY => {
                        if self.on_syscall_entry(p)? {
                            // The syscall parks this thread (a futex wait):
                            // run someone else instead.
                            self.current = None;
                        } else {
                            self.resume_current(p)?;
                        }
                    }
                    PTRACE_SYSCALL_INFO_EXIT => {
                        self.on_syscall_exit(p)?;
                        // Every syscall boundary is a scheduling point.
                        self.yield_current(p)?;
                    }
                    _ => self.resume_current(p)?,
                }
            }
            WaitStatus::PtraceEvent(p, _sig, event) => match event {
                libc::PTRACE_EVENT_CLONE | libc::PTRACE_EVENT_FORK | libc::PTRACE_EVENT_VFORK => {
                    let child =
                        Pid::from_raw(ptrace::getevent(p).map_err(SuperviseError::Ptrace)? as i32);
                    let info = self.clone_pending.remove(&p).unwrap_or_default();
                    self.register_thread(child, info)?;
                    // Release the child deterministically: wait for its attach
                    // stop here, so the child joins the runnable queue at a
                    // point fixed by the parent's syscall stream (the kernel
                    // delivers the stop at an otherwise racy time).
                    if let Some(code) = self.await_attach(child)? {
                        return Ok(Some(code));
                    }
                    if self.is_scheduled(p) {
                        self.resume_current(p)?;
                    } else {
                        self.resume_thread(p)?;
                    }
                }
                libc::PTRACE_EVENT_EXIT => {
                    // The thread is dying; let it reach its final state so
                    // it can be reaped (the CLEARTID wake is performed once
                    // it has actually exited, when the tid word is cleared).
                    self.resume_thread(p)?;
                }
                _ => {
                    if self.is_scheduled(p) {
                        self.resume_current(p)?;
                    } else {
                        self.resume_thread(p)?;
                    }
                }
            },
            WaitStatus::Stopped(p, sig) => {
                if sig == Signal::SIGSTOP {
                    // The initial attach stop of a newly-created task. It is
                    // normally consumed by `await_attach` right at the parent's
                    // clone event; this handler only covers the two orders the
                    // kernel can report the stop in. The init's own stop is
                    // resumed below, an unregistered child is parked until its
                    // clone event registers it.
                    if self.current == Some(p) {
                        self.resume_thread(p)?;
                    } else if self.pending_start.remove(&p) {
                        if self.is_scheduled(p) {
                            self.runnable.push_back(p);
                        } else {
                            self.resume_thread(p)?;
                        }
                    } else if !self.threads.contains_key(&p) {
                        self.stopped_pending.insert(p);
                    }
                    // else: a registered, already-runable thread: keep it
                    // parked until the scheduler picks it.
                } else if self.current == Some(p) {
                    // Ordinary signal-stop on the running thread: deliver.
                    ptrace::syscall(p, Some(sig)).map_err(SuperviseError::Ptrace)?;
                } else if self.is_scheduled(p) {
                    self.sig_pending.insert(p, sig);
                } else if self.threads.contains_key(&p) {
                    // Signal for a registered concurrent process: deliver
                    // directly.
                    ptrace::syscall(p, Some(sig)).map_err(SuperviseError::Ptrace)?;
                }
                // else: a signal for an unregistered child; leave it
                // stopped until its clone event registers it.
            }
            WaitStatus::Exited(p, code) if p == self.pid => {
                self.trace_lifecycle("exit", code);
                return Ok(Some(code));
            }
            WaitStatus::Signaled(p, sig, _core) if p == self.pid => {
                self.trace_lifecycle("signal", sig as i32);
                return Ok(Some(128 + sig as i32));
            }
            WaitStatus::Exited(p, _) | WaitStatus::Signaled(p, _, _) => {
                let was_current = self.current == Some(p);
                self.on_thread_exit(p, was_current);
                if was_current && !self.group_exiting {
                    self.current = None;
                    self.schedule_next()?;
                }
            }
            WaitStatus::Continued(_) | WaitStatus::StillAlive => {}
        }
        Ok(None)
    }

    /// Block until the attach stop of a newly-registered child has been
    /// observed, then release it (scheduled threads join the runnable queue,
    /// concurrent processes are resumed). This pins the child's release to a
    /// point determined by the parent's syscall stream, which makes the FIFO
    /// schedule reproducible run-to-run. Other statuses received while waiting
    /// are handled normally.
    fn await_attach(&mut self, child: Pid) -> Result<Option<i32>, SuperviseError> {
        if !self.pending_start.contains(&child) {
            // The attach stop was already observed before the clone event.
            return Ok(None);
        }
        loop {
            let status = self.wait()?;
            if let WaitStatus::Stopped(p, sig) = status
                && p == child
                && sig == Signal::SIGSTOP
            {
                self.pending_start.remove(&child);
                if self.is_scheduled(child) {
                    self.runnable.push_back(child);
                } else {
                    self.resume_thread(child)?;
                }
                return Ok(None);
            }
            if let Some(code) = self.dispatch(status)? {
                return Ok(Some(code));
            }
        }
    }

    /// Pick the next runnable thread, make it current and resume it. Records a
    /// `schedule` event when control actually moves between threads. Returns
    /// `Ok(())` even when nothing is runnable right now: some ptrace stop is
    /// still pending (a new thread's attach stop, or a concurrent process's
    /// syscall) that the waitpid loop will deliver next.
    fn schedule_next(&mut self) -> Result<(), SuperviseError> {
        loop {
            let Some(tid) = self.runnable.pop_front() else {
                if !self.pending_start.is_empty() || self.threads.values().any(|t| !t.scheduled) {
                    // A stop is still expected; go back to waitpid rather than
                    // declaring a deadlock.
                    return Ok(());
                }
                return Err(SuperviseError::Deadlock);
            };
            let Some(thread) = self.threads.get(&tid) else {
                continue;
            };
            if self.blocked.contains(&tid) {
                continue;
            }
            let to = thread.vtid;
            let from = self.last_vtid;
            if from != Some(to) {
                self.trace_json("schedule", &json!({ "from": from, "to": to }));
            }
            self.last_vtid = Some(to);
            self.current = Some(tid);
            let sig = self.sig_pending.remove(&tid);
            ptrace::syscall(tid, sig).map_err(SuperviseError::Ptrace)?;
            return Ok(());
        }
    }

    /// After a syscall exit, rotate the current thread to the back of the
    /// runnable queue and schedule the next one.
    fn yield_current(&mut self, p: Pid) -> Result<(), SuperviseError> {
        if self.current == Some(p) {
            self.current = None;
            self.runnable.push_back(p);
        }
        self.schedule_next()
    }

    fn resume_current(&mut self, p: Pid) -> Result<(), SuperviseError> {
        if self.current == Some(p) {
            self.resume_thread(p)?;
        }
        Ok(())
    }

    /// Whether `p` is a task sharing the init's address space and therefore
    /// subject to the deterministic single-runner scheduler.
    fn is_scheduled(&self, p: Pid) -> bool {
        self.threads.get(&p).is_some_and(|t| t.scheduled)
    }

    fn resume_thread(&mut self, p: Pid) -> Result<(), SuperviseError> {
        let sig = self.sig_pending.remove(&p);
        ptrace::syscall(p, sig).map_err(SuperviseError::Ptrace)?;
        Ok(())
    }

    /// Register a new thread (or fork child) reported by a ptrace clone event.
    /// It is only released once its attach SIGSTOP stop has been observed: a
    /// scheduled thread then joins the runnable queue, a concurrent process is
    /// resumed so it can start running. If the stop has not arrived yet, the
    /// pid is parked in `pending_start` and released by the SIGSTOP handler.
    fn register_thread(&mut self, pid: Pid, info: CloneInfo) -> Result<(), SuperviseError> {
        if self.threads.contains_key(&pid) {
            return Ok(());
        }
        let vtid = self.vtid_next;
        self.vtid_next += 1;
        let scheduled = (info.flags & libc::CLONE_THREAD as u64) != 0;
        self.threads.insert(
            pid,
            Thread {
                vtid,
                child_tid: info.child_tid,
                flags: info.flags,
                scheduled,
            },
        );
        if self.stopped_pending.remove(&pid) {
            // The attach stop arrived before the clone event: release now.
            if scheduled {
                self.runnable.push_back(pid);
            } else {
                self.resume_thread(pid)?;
            }
        } else {
            self.pending_start.insert(pid);
        }
        self.trace_json(
            "thread_create",
            &json!({ "tid": vtid, "scheduled": scheduled }),
        );
        Ok(())
    }

    /// Remove a thread that exited or was killed. When `record` is set (the
    /// thread was the current one) a `thread_exit` event is traced; threads
    /// torn down by `exit_group` are reaped without trace so the event stream
    /// stays independent of the kernel's (nondeterministic) kill order.
    fn on_thread_exit(&mut self, pid: Pid, record: bool) {
        if let Some(t) = self.threads.remove(&pid) {
            if t.child_tid != 0 && (t.flags & libc::CLONE_CHILD_CLEARTID as u64) != 0 {
                // The kernel clears the tid word and wakes one waiter (the
                // pthread_join fast path) once the thread is gone.
                self.wake(t.child_tid, 1);
            }
            if record {
                self.trace_json("thread_exit", &json!({ "tid": t.vtid }));
            }
            self.pending.remove(&pid);
        }
        self.runnable.retain(|&p| p != pid);
        self.blocked.remove(&pid);
        self.waiting
            .values_mut()
            .for_each(|q| q.retain(|&p| p != pid));
        self.waiting.retain(|_, q| !q.is_empty());
        self.sig_pending.remove(&pid);
        self.pending_start.remove(&pid);
        self.stopped_pending.remove(&pid);
    }

    /// Park the current thread on a simulated futex wait for `addr`.
    fn block(&mut self, pid: Pid, addr: u64) {
        self.blocked.insert(pid);
        self.waiting.entry(addr).or_default().push_back(pid);
    }

    /// Wake up to `val` threads parked on `addr`, moving them to the runnable
    /// queue (FIFO). Returns how many were woken.
    fn wake(&mut self, addr: u64, val: u64) -> usize {
        let cap = (val as i64).max(0) as usize;
        if cap == 0 {
            return 0;
        }
        let Some(mut parked) = self.waiting.remove(&addr) else {
            return 0;
        };
        let mut woken = 0;
        while woken < cap {
            let Some(t) = parked.pop_front() else { break };
            if self.blocked.remove(&t) {
                self.runnable.push_back(t);
                woken += 1;
            }
        }
        if !parked.is_empty() {
            self.waiting.insert(addr, parked);
        }
        woken
    }

    fn on_syscall_entry(&mut self, p: Pid) -> Result<bool, SuperviseError> {
        let regs = ptrace::getregs(p).map_err(SuperviseError::Ptrace)?;
        let nr = regs.orig_rax as i64;

        match nr {
            libc::SYS_getrandom => self.intercept_getrandom(p, &regs)?,
            libc::SYS_clock_gettime | SYS_CLOCK_GETTIME64 => {
                self.intercept_clock_gettime(p, &regs)?
            }
            libc::SYS_gettimeofday => self.intercept_gettimeofday(p, &regs)?,
            libc::SYS_futex => return self.intercept_futex(p, &regs),
            libc::SYS_exit_group => {
                self.group_exiting = true;
                self.trace_syscall(nr, "pass");
            }
            libc::SYS_clone | libc::SYS_clone3 => {
                self.stash_clone(p, nr, &regs)?;
                self.trace_syscall(nr, "pass");
            }
            _ => self.trace_syscall(nr, "pass"),
        }
        Ok(false)
    }

    fn on_syscall_exit(&mut self, p: Pid) -> Result<(), SuperviseError> {
        if let Some(inject) = self.pending.remove(&p) {
            let mut regs = ptrace::getregs(p).map_err(SuperviseError::Ptrace)?;
            regs.rax = inject.retval as u64;
            ptrace::setregs(p, regs).map_err(SuperviseError::Ptrace)?;
            self.trace_syscall(inject.syscall, "inject");
        }
        Ok(())
    }

    /// Record the `clone`/`clone3` arguments needed to emulate the kernel's
    /// thread-exit behaviour once the new thread is registered.
    fn stash_clone(
        &mut self,
        p: Pid,
        nr: i64,
        regs: &nix::libc::user_regs_struct,
    ) -> Result<(), SuperviseError> {
        let (flags, child_tid) = if nr == libc::SYS_clone3 {
            match read_proc_mem(p, regs.rdi, 24).map_err(SuperviseError::Io) {
                // struct clone_args: flags@0, pidfd@8, child_tid@16.
                Ok(buf) if buf.len() == 24 => (le_u64(&buf[..8]), le_u64(&buf[16..24])),
                Ok(_) | Err(_) => (0, 0),
            }
        } else {
            // clone(flags, stack, parent_tid, child_tid, tls): rdi..r10.
            (regs.rdi, regs.r10)
        };
        self.clone_pending.insert(p, CloneInfo { flags, child_tid });
        Ok(())
    }

    /// Interpose `futex` so a wait never blocks in the kernel (which would
    /// stall the single-runner scheduler). Returns `true` when the calling
    /// thread is parked and the scheduler must run another thread.
    ///
    /// When fewer than two threads share the init's address space there is no
    /// interleaving to make deterministic, so futexes are passed through to the
    /// kernel (which handles their possibly timed waits correctly).
    fn intercept_futex(
        &mut self,
        p: Pid,
        regs: &nix::libc::user_regs_struct,
    ) -> Result<bool, SuperviseError> {
        let uaddr = regs.rdi;
        let val = regs.rdx;
        let cmd = (regs.rsi as i32) & FUTEX_CMD_MASK;

        if !self.is_scheduled(p) || self.threads.values().filter(|t| t.scheduled).count() <= 1 {
            self.trace_futex(p, futex_cmd_name(cmd), "pass");
            return Ok(false);
        }

        match cmd {
            libc::FUTEX_WAIT | libc::FUTEX_WAIT_BITSET => {
                if read_u32(p, uaddr).ok() == Some(val as u32) {
                    // The word still holds the expected value: park the thread.
                    self.skip_syscall(p, libc::SYS_futex, 0)?;
                    self.block(p, uaddr);
                    self.trace_futex(p, futex_cmd_name(cmd), "wait");
                    return Ok(true);
                }
                // The word changed: the kernel would return EAGAIN immediately.
                self.trace_futex(p, futex_cmd_name(cmd), "pass");
                Ok(false)
            }
            libc::FUTEX_WAKE | libc::FUTEX_WAKE_BITSET => {
                let n = self.wake(uaddr, val);
                self.skip_syscall(p, libc::SYS_futex, n as i64)?;
                self.trace_futex(p, futex_cmd_name(cmd), "wake");
                Ok(false)
            }
            libc::FUTEX_REQUEUE | libc::FUTEX_CMP_REQUEUE => {
                if cmd == libc::FUTEX_CMP_REQUEUE {
                    let val3 = regs.r9 as u32;
                    if read_u32(p, uaddr).ok() != Some(val3) {
                        // Would return EAGAIN without waking anyone.
                        self.trace_futex(p, futex_cmd_name(cmd), "pass");
                        return Ok(false);
                    }
                }
                // Requeue behaves like a wake for correctness purposes: waking
                // the requeued waiter yields only a (legal) spurious wakeup.
                let val2 = regs.r10;
                let mut n = self.wake(uaddr, val);
                n += self.wake(uaddr, val2);
                self.skip_syscall(p, libc::SYS_futex, n as i64)?;
                self.trace_futex(p, futex_cmd_name(cmd), "wake");
                Ok(false)
            }
            _ => {
                self.trace_futex(p, futex_cmd_name(cmd), "pass");
                Ok(false)
            }
        }
    }

    /// Skip the syscall under way and supply `retval` at its exit stop.
    fn skip_syscall(&mut self, p: Pid, syscall: i64, retval: i64) -> Result<(), SuperviseError> {
        let mut regs = ptrace::getregs(p).map_err(SuperviseError::Ptrace)?;
        regs.orig_rax = -1i64 as u64;
        ptrace::setregs(p, regs).map_err(SuperviseError::Ptrace)?;
        self.pending.insert(p, PendingInject { syscall, retval });
        Ok(())
    }

    /// Replace `getrandom(2)` output with deterministic bytes drawn from the
    /// seeded PRNG and skip the kernel's entropy source entirely.
    fn intercept_getrandom(
        &mut self,
        p: Pid,
        regs: &nix::libc::user_regs_struct,
    ) -> Result<(), SuperviseError> {
        let buf = regs.rdi;
        let buflen = regs.rsi.min(1 << 20);

        let mut out = vec![0u8; buflen as usize];
        self.rng.fill(&mut out);

        if buflen > 0 {
            write_proc_mem(p, buf, &out).map_err(SuperviseError::Io)?;
        }

        self.skip_syscall(p, libc::SYS_getrandom, buflen as i64)?;

        self.trace_json(
            "getrandom",
            &json!({ "nbytes": buflen, "bytes": hex_bytes(&out) }),
        );
        Ok(())
    }

    /// Replace `clock_gettime(2)` output with the deterministic virtual clock.
    fn intercept_clock_gettime(
        &mut self,
        p: Pid,
        regs: &nix::libc::user_regs_struct,
    ) -> Result<(), SuperviseError> {
        let clk = regs.rdi as i32;
        let ts_ptr = regs.rsi;
        let (sec, nsec) = self.virtual_clock(clk);

        let mut ts = [0u8; 16];
        ts[..8].copy_from_slice(&sec.to_le_bytes());
        ts[8..].copy_from_slice(&nsec.to_le_bytes());
        write_proc_mem(p, ts_ptr, &ts).map_err(SuperviseError::Io)?;

        self.skip_syscall(p, libc::SYS_clock_gettime, 0)?;

        self.trace_json(
            "clock_gettime",
            &json!({ "clock": clk, "sec": sec, "nsec": nsec }),
        );
        Ok(())
    }

    /// Replace `gettimeofday(2)` output with the frozen virtual wall clock.
    fn intercept_gettimeofday(
        &mut self,
        p: Pid,
        regs: &nix::libc::user_regs_struct,
    ) -> Result<(), SuperviseError> {
        let tv_ptr = regs.rdi;
        let (sec, nsec) = self.virtual_clock(CLOCK_REALTIME);

        let mut tv = [0u8; 16];
        tv[..8].copy_from_slice(&sec.to_le_bytes());
        tv[8..].copy_from_slice(&(nsec / 1_000).to_le_bytes());
        write_proc_mem(p, tv_ptr, &tv).map_err(SuperviseError::Io)?;

        self.skip_syscall(p, libc::SYS_gettimeofday, 0)?;

        self.trace_json("gettimeofday", &json!({ "sec": sec, "usec": nsec / 1_000 }));
        Ok(())
    }

    /// Deterministic virtual clock: realtime is frozen at the seeded epoch,
    /// monotonic time advances by a fixed step per read.
    fn virtual_clock(&mut self, clk: i32) -> (u64, u64) {
        match clk {
            CLOCK_REALTIME | CLOCK_REALTIME_COARSE => (VIRTUAL_EPOCH, 0),
            CLOCK_MONOTONIC | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME => {
                let t = self.tick;
                self.tick += 1;
                (
                    t * MONO_STEP_NS / 1_000_000_000,
                    (t * MONO_STEP_NS) % 1_000_000_000,
                )
            }
            _ => (VIRTUAL_EPOCH, 0),
        }
    }

    fn trace_futex(&mut self, p: Pid, op: &str, kind: &str) {
        let tid = self.threads.get(&p).map(|t| t.vtid);
        self.trace_json("futex", &json!({ "op": op, "kind": kind, "tid": tid }));
    }

    fn trace_syscall(&mut self, nr: i64, kind: &str) {
        self.trace_json(syscall_name(nr), &json!({ "kind": kind }));
    }

    fn trace_lifecycle(&mut self, event: &str, code: i32) {
        self.trace_json(event, &json!({ "code": code }));
    }

    fn trace_json(&mut self, event: &str, detail: &serde_json::Value) {
        self.seq += 1;
        let rec = json!({
            "seq": self.seq,
            "event": event,
            "detail": detail,
        });
        state::trace_event(&self.state_root, &self.id, &rec);
    }
}

fn futex_cmd_name(cmd: i32) -> &'static str {
    match cmd {
        libc::FUTEX_WAIT => "wait",
        libc::FUTEX_WAKE => "wake",
        libc::FUTEX_REQUEUE => "requeue",
        libc::FUTEX_CMP_REQUEUE => "cmp_requeue",
        libc::FUTEX_WAIT_BITSET => "wait_bitset",
        libc::FUTEX_WAKE_BITSET => "wake_bitset",
        _ => "other",
    }
}

/// Append `data` into the tracee's memory at `addr` via `/proc/<pid>/mem`.
/// The tracee is held stopped at a syscall boundary by the tracer, so the
/// write is race-free.
fn write_proc_mem(pid: Pid, addr: u64, data: &[u8]) -> std::io::Result<()> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("/proc/{}/mem", pid.as_raw()))?;
    file.write_at(data, addr)?;
    Ok(())
}

/// Read `len` bytes from the tracee's memory at `addr` via `/proc/<pid>/mem`.
fn read_proc_mem(pid: Pid, addr: u64, len: usize) -> std::io::Result<Vec<u8>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .open(format!("/proc/{}/mem", pid.as_raw()))?;
    let mut buf = vec![0u8; len];
    file.read_exact_at(&mut buf, addr)?;
    Ok(buf)
}

fn read_u32(pid: Pid, addr: u64) -> std::io::Result<u32> {
    let buf = read_proc_mem(pid, addr, 4)?;
    Ok(le_u32(&buf))
}

fn le_u64(bytes: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    let n = bytes.len().min(8);
    b[..n].copy_from_slice(&bytes[..n]);
    u64::from_le_bytes(b)
}

fn le_u32(bytes: &[u8]) -> u32 {
    let mut b = [0u8; 4];
    let n = bytes.len().min(4);
    b[..n].copy_from_slice(&bytes[..n]);
    u32::from_le_bytes(b)
}

/// Human-readable name for a syscall number (best effort; falls back to `nr`).
fn syscall_name(nr: i64) -> &'static str {
    match nr {
        libc::SYS_getrandom => "getrandom",
        libc::SYS_clock_gettime => "clock_gettime",
        SYS_CLOCK_GETTIME64 => "clock_gettime64",
        libc::SYS_gettimeofday => "gettimeofday",
        libc::SYS_read => "read",
        libc::SYS_write => "write",
        libc::SYS_openat => "openat",
        libc::SYS_close => "close",
        libc::SYS_mmap => "mmap",
        libc::SYS_munmap => "munmap",
        libc::SYS_futex => "futex",
        libc::SYS_clone => "clone",
        libc::SYS_clone3 => "clone3",
        libc::SYS_execve => "execve",
        libc::SYS_execveat => "execveat",
        libc::SYS_exit => "exit",
        libc::SYS_exit_group => "exit_group",
        libc::SYS_nanosleep => "nanosleep",
        libc::SYS_brk => "brk",
        libc::SYS_arch_prctl => "arch_prctl",
        libc::SYS_set_tid_address => "set_tid_address",
        libc::SYS_prctl => "prctl",
        libc::SYS_newfstatat => "newfstatat",
        libc::SYS_lseek => "lseek",
        libc::SYS_ioctl => "ioctl",
        libc::SYS_poll => "poll",
        libc::SYS_ppoll => "ppoll",
        libc::SYS_rt_sigaction => "rt_sigaction",
        libc::SYS_rt_sigprocmask => "rt_sigprocmask",
        libc::SYS_socket => "socket",
        libc::SYS_connect => "connect",
        libc::SYS_accept => "accept",
        libc::SYS_bind => "bind",
        libc::SYS_getsockname => "getsockname",
        libc::SYS_dup2 => "dup2",
        libc::SYS_fcntl => "fcntl",
        libc::SYS_getpid => "getpid",
        libc::SYS_getppid => "getppid",
        libc::SYS_getuid => "getuid",
        libc::SYS_setuid => "setuid",
        libc::SYS_getgid => "getgid",
        libc::SYS_setgid => "setgid",
        libc::SYS_readlinkat => "readlinkat",
        libc::SYS_mkdirat => "mkdirat",
        libc::SYS_unlinkat => "unlinkat",
        libc::SYS_mount => "mount",
        libc::SYS_chdir => "chdir",
        libc::SYS_set_robust_list => "set_robust_list",
        libc::SYS_rseq => "rseq",
        libc::SYS_pipe2 => "pipe2",
        libc::SYS_epoll_create1 => "epoll_create1",
        libc::SYS_epoll_ctl => "epoll_ctl",
        libc::SYS_epoll_wait => "epoll_wait",
        libc::SYS_getdents64 => "getdents64",
        libc::SYS_sched_yield => "sched_yield",
        libc::SYS_madvise => "madvise",
        libc::SYS_mprotect => "mprotect",
        libc::SYS_membarrier => "membarrier",
        _ => "",
    }
}
