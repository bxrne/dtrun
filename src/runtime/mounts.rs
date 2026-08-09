//! Mount point handling: translate OCI `mounts` entries into `mount(2)` calls,
//! and provide the storage-isolation primitives for the container root.

use crate::oci::config::Mount;
use nix::errno::Errno;
use nix::mount::{MsFlags, mount};
use std::ffi::CString;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;

/// Errors produced while applying mounts.
#[derive(Debug)]
pub enum MountError {
    /// The mount point directory could not be created.
    Mkdir(std::io::Error),
    /// The `mount(2)` call failed.
    Mount(Errno),
}

impl std::fmt::Display for MountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MountError::Mkdir(e) => write!(f, "mkdir failed: {e}"),
            MountError::Mount(e) => write!(f, "mount failed: {e}"),
        }
    }
}

impl std::error::Error for MountError {}

/// Apply a single OCI mount entry inside the container.
///
/// Runs after `chroot`, so `destination` is relative to the new root.
/// The mount point is created if it does not yet exist.
pub fn apply_mount(m: &Mount) -> Result<(), MountError> {
    let source = m.source.as_deref().unwrap_or("");
    let fstype = m.fstype.as_deref();
    let data = parse_mount_data(m);
    let mut flags = parse_mount_flags(m);

    let dest = m.destination.as_str();

    // procfs is mounted read-only when the config does not ask for rw.
    if fstype == Some("proc")
        && !flags.contains(MsFlags::MS_RDONLY)
        && !m
            .options
            .as_ref()
            .is_some_and(|o| o.iter().any(|x| x == "rw"))
    {
        flags |= MsFlags::MS_RDONLY;
    }

    if !Path::new(dest).exists() {
        std::fs::create_dir_all(dest).map_err(MountError::Mkdir)?;
    }

    mount(Some(source), dest, fstype, flags, data.as_deref()).map_err(MountError::Mount)
}

/// Parse the mount `options` list into a comma-separated data string for the
/// fields `mount(2)` does not take as flags (e.g. `mode`, `uid`, `gid`).
fn parse_mount_data(m: &Mount) -> Option<String> {
    let opts = m.options.as_ref()?;
    let data: Vec<&str> = opts
        .iter()
        .filter(|opt| {
            let o = opt.as_str();
            !matches!(
                o,
                "nosuid"
                    | "noexec"
                    | "nodev"
                    | "ro"
                    | "rw"
                    | "rbind"
                    | "bind"
                    | "remount"
                    | "suid"
                    | "exec"
                    | "dev"
                    | "nosymfollow"
                    | "private"
                    | "rprivate"
                    | "shared"
                    | "rshared"
                    | "slave"
                    | "rslave"
                    | "unbindable"
                    | "runbindable"
                    | "relatime"
                    | "norelatime"
                    | "strictatime"
                    | "nostrictatime"
                    | "lazytime"
                    | "nolazytime"
            )
        })
        .map(|o| o.as_str())
        .collect();
    if data.is_empty() {
        None
    } else {
        Some(data.join(","))
    }
}

/// Translate the mount `options` list into `MS_*` flags.
fn parse_mount_flags(m: &Mount) -> MsFlags {
    let mut flags = MsFlags::empty();
    let Some(opts) = &m.options else {
        return flags;
    };
    for opt in opts {
        match opt.as_str() {
            "nosuid" => flags |= MsFlags::MS_NOSUID,
            "noexec" => flags |= MsFlags::MS_NOEXEC,
            "nodev" => flags |= MsFlags::MS_NODEV,
            "ro" => flags |= MsFlags::MS_RDONLY,
            "rbind" => flags |= MsFlags::MS_BIND | MsFlags::MS_REC,
            "bind" => flags |= MsFlags::MS_BIND,
            "remount" => flags |= MsFlags::MS_REMOUNT,
            "suid" => flags &= !MsFlags::MS_NOSUID,
            "exec" => flags &= !MsFlags::MS_NOEXEC,
            "dev" => flags &= !MsFlags::MS_NODEV,
            "private" => flags |= MsFlags::MS_PRIVATE,
            "rprivate" => flags |= MsFlags::MS_REC | MsFlags::MS_PRIVATE,
            "shared" => flags |= MsFlags::MS_SHARED,
            "rshared" => flags |= MsFlags::MS_REC | MsFlags::MS_SHARED,
            "slave" => flags |= MsFlags::MS_SLAVE,
            "rslave" => flags |= MsFlags::MS_REC | MsFlags::MS_SLAVE,
            "unbindable" => flags |= MsFlags::MS_UNBINDABLE,
            "runbindable" => flags |= MsFlags::MS_REC | MsFlags::MS_UNBINDABLE,
            _ => {}
        }
    }
    flags
}

/// Remount the root mount so host mount propagation doesn't leak into the
/// container's mount namespace.
pub fn make_root_private() -> Result<(), Errno> {
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )
}

/// Make the container root filesystem read-only (when `root.readonly` is set).
pub fn make_root_readonly() -> Result<(), Errno> {
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
        None::<&str>,
    )
}

/// Bind-mount the loopback interface data file used by the deterministic
/// runtime; reserved for deterministic network setup.
pub fn bind_mount(source: &str, dest: &str) -> Result<(), Errno> {
    mount(
        Some(source),
        dest,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
}

/// Create the OCI default device nodes and `/dev` symlinks inside the
/// container (runtime-spec Linux "default devices"). Runs after the `/dev`
/// tmpfs is mounted. Rootless runtimes cannot `mknod(2)` device nodes (the
/// kernel forbids it inside a user namespace), so the host's nodes — opened
/// before `chroot` and passed in as `(name, fd)` pairs — are bind-mounted in
/// via `/proc/self/fd/<n>`.
pub fn setup_default_devices(host_devices: &[(String, OwnedFd)]) {
    for (name, fd) in host_devices {
        let dest = format!("/dev/{name}");
        let source = format!("/proc/self/fd/{}", fd.as_raw_fd());
        // A placeholder file provides the mount point for the bind.
        let _ = std::fs::File::create(&dest);
        if let Err(e) = bind_mount(&source, &dest) {
            tracing::debug!(dest, ?e, "bind device failed");
        }
    }

    const LINKS: &[(&str, &str)] = &[
        ("/dev/fd", "/proc/self/fd"),
        ("/dev/stdin", "/proc/self/fd/0"),
        ("/dev/stdout", "/proc/self/fd/1"),
        ("/dev/stderr", "/proc/self/fd/2"),
        ("/dev/ptmx", "pts/ptmx"),
    ];
    for (link, target) in LINKS {
        let (c_link, c_target) = match (CString::new(*link), CString::new(*target)) {
            (Ok(l), Ok(t)) => (l, t),
            _ => continue,
        };
        // Safety: creating a symlink inside the container root.
        let ret = unsafe { libc::symlink(c_target.as_ptr(), c_link.as_ptr()) };
        if ret != 0 {
            tracing::debug!(link, errno = Errno::last_raw(), "symlink failed");
        }
    }
}
