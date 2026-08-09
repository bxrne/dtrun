//! Deterministic supervisor: drives the container under `PTRACE_SYSCALL`,
//! intercepting the sources of nondeterminism the kernel exposes to the
//! workload — randomness (`getrandom`) and wall/monotonic time
//! (`clock_gettime`, `gettimeofday`) — and recording an execution event trace
//! for later replay.

use crate::runtime::state;
use nix::errno::Errno;
use nix::sys::ptrace;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use serde_json::json;
use std::collections::HashMap;
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
}

impl std::fmt::Display for SuperviseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SuperviseError::Ptrace(e) => write!(f, "ptrace failed: {e}"),
            SuperviseError::Wait(e) => write!(f, "waitpid failed: {e}"),
            SuperviseError::Io(e) => write!(f, "io failed: {e}"),
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
    };
    sv.run()
}

struct Supervisor {
    pid: Pid,
    rng: Prng,
    tick: u64,
    pending: HashMap<Pid, PendingInject>,
    seq: u64,
    state_root: std::path::PathBuf,
    id: String,
}

impl Supervisor {
    fn run(&mut self) -> Result<i32, SuperviseError> {
        // Release the container from the `created` pause (its SIGSTOP) without
        // delivering the signal; the next stops are syscall boundaries.
        ptrace::syscall(self.pid, None).map_err(SuperviseError::Ptrace)?;

        loop {
            let status = waitpid(
                Pid::from_raw(-1),
                Some(WaitPidFlag::__WALL | WaitPidFlag::WUNTRACED),
            )
            .map_err(SuperviseError::Wait)?;

            match status {
                WaitStatus::PtraceSyscall(p) => {
                    let info = ptrace::syscall_info(p).map_err(SuperviseError::Ptrace)?;
                    match info.op {
                        PTRACE_SYSCALL_INFO_ENTRY => self.on_syscall_entry(p)?,
                        PTRACE_SYSCALL_INFO_EXIT => self.on_syscall_exit(p)?,
                        _ => {}
                    }
                    ptrace::syscall(p, None).map_err(SuperviseError::Ptrace)?;
                }
                WaitStatus::PtraceEvent(p, _sig, _event) => {
                    // fork/vfork/clone/exec/exit bookkeeping: keep going.
                    ptrace::syscall(p, None).map_err(SuperviseError::Ptrace)?;
                }
                WaitStatus::Stopped(p, sig) => {
                    // Ordinary signal-stop: deliver the signal to the container.
                    ptrace::syscall(p, Some(sig)).map_err(SuperviseError::Ptrace)?;
                }
                WaitStatus::Exited(p, code) if p == self.pid => {
                    self.trace_lifecycle("exit", code);
                    return Ok(code);
                }
                WaitStatus::Signaled(p, sig, _core) if p == self.pid => {
                    self.trace_lifecycle("signal", sig as i32);
                    return Ok(128 + sig as i32);
                }
                // A child of the container init exited; keep supervising until
                // the init itself terminates.
                WaitStatus::Exited(_p, _) | WaitStatus::Signaled(_p, _, _) => {}
                WaitStatus::Continued(_) | WaitStatus::StillAlive => {}
            }
        }
    }

    fn on_syscall_entry(&mut self, p: Pid) -> Result<(), SuperviseError> {
        let regs = ptrace::getregs(p).map_err(SuperviseError::Ptrace)?;
        let nr = regs.orig_rax as i64;

        match nr {
            libc::SYS_getrandom => self.intercept_getrandom(p, &regs)?,
            libc::SYS_clock_gettime | SYS_CLOCK_GETTIME64 => {
                self.intercept_clock_gettime(p, &regs)?
            }
            libc::SYS_gettimeofday => self.intercept_gettimeofday(p, &regs)?,
            _ => self.trace_syscall(nr, "pass"),
        }
        Ok(())
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

        self.pending.insert(
            p,
            PendingInject {
                syscall: libc::SYS_getrandom,
                retval: buflen as i64,
            },
        );

        let mut regs = *regs;
        regs.orig_rax = -1i64 as u64;
        ptrace::setregs(p, regs).map_err(SuperviseError::Ptrace)?;

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

        self.pending.insert(
            p,
            PendingInject {
                syscall: libc::SYS_clock_gettime,
                retval: 0,
            },
        );

        let mut regs = *regs;
        regs.orig_rax = -1i64 as u64;
        ptrace::setregs(p, regs).map_err(SuperviseError::Ptrace)?;

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

        self.pending.insert(
            p,
            PendingInject {
                syscall: libc::SYS_gettimeofday,
                retval: 0,
            },
        );

        let mut regs = *regs;
        regs.orig_rax = -1i64 as u64;
        ptrace::setregs(p, regs).map_err(SuperviseError::Ptrace)?;

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
