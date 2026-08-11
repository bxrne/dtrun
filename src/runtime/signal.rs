//! Signal parsing and utilities.

use nix::sys::signal::Signal;
use std::fmt;

/// Errors produced when parsing signal names.
#[derive(Debug)]
pub enum SignalError {
    BadSignal(String),
}

impl fmt::Display for SignalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SignalError::BadSignal(s) => write!(f, "unknown signal '{s}'"),
        }
    }
}

impl std::error::Error for SignalError {}

/// Parse a signal name (e.g. "SIGTERM", "TERM", "9") into a [`Signal`].
pub fn parse_signal(sig: &str) -> Result<Signal, SignalError> {
    let s = sig.trim().to_uppercase();
    let s = s.strip_prefix("SIG").unwrap_or(&s);
    if let Ok(n) = s.parse::<i32>() {
        if let Ok(signal) = Signal::try_from(n) {
            return Ok(signal);
        }
        return Err(SignalError::BadSignal(sig.into()));
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
        .ok_or_else(|| SignalError::BadSignal(sig.into()))
}
