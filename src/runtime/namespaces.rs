//! User-namespace plumbing: id mappings and credential drops for the container.

use nix::errno::Errno;
use nix::unistd::Pid;
use std::fmt;
use std::io::Write;

/// Errors encountered while setting up the child's user namespace.
#[derive(Debug)]
pub enum NamespaceError {
    /// Writing `/proc/<pid>/{setgroups,gid_map,uid_map}` failed.
    IdMap(String, std::io::Error),
    /// A credential syscall (setgroups/setuid/setgid) failed.
    Credential(&'static str, Errno),
}

impl fmt::Display for NamespaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NamespaceError::IdMap(entry, e) => write!(f, "write {entry} failed: {e}"),
            NamespaceError::Credential(op, e) => write!(f, "{op} failed: {e}"),
        }
    }
}

impl std::error::Error for NamespaceError {}

/// Write a single line into one of `/proc/<pid>/{setgroups,gid_map,uid_map}`.
///
/// The kernel only permits these writes once, in the order `setgroups` ->
/// `gid_map` -> `uid_map`. For an unprivileged runtime the caller must be the
/// parent of the process that entered the user namespace.
pub fn write_id_map(pid: Pid, entry: &str, contents: &[u8]) -> std::io::Result<()> {
    let path = format!("/proc/{}/{entry}", pid.as_raw());
    let mut f = std::fs::File::create(path)?;
    f.write_all(contents)
}

/// Map the child (already inside its own user namespace) to uid/gid 0.
///
/// Must run in the *parent*: it writes `/proc/<child>/*` while the child waits.
/// After this the child is root inside its namespace but remains an
/// unprivileged user on the host.
pub fn map_root_user(child: Pid) -> Result<(), NamespaceError> {
    let uid = nix::unistd::getuid();
    let gid = nix::unistd::getgid();
    write_id_map(child, "setgroups", b"deny\n")
        .map_err(|e| NamespaceError::IdMap("setgroups".into(), e))?;
    write_id_map(
        child,
        "gid_map",
        &format!("0 {} 1\n", gid.as_raw()).into_bytes(),
    )
    .map_err(|e| NamespaceError::IdMap("gid_map".into(), e))?;
    write_id_map(
        child,
        "uid_map",
        &format!("0 {} 1\n", uid.as_raw()).into_bytes(),
    )
    .map_err(|e| NamespaceError::IdMap("uid_map".into(), e))?;
    Ok(())
}
