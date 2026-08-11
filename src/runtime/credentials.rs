//! Process credential handling: apply the OCI `process.capabilities` sets and
//! `process.oomScoreAdj` to the container init, after its user namespace and
//! mounts are in place.

use crate::oci::config::Capabilities;
use nix::errno::Errno;
use std::io::Write;

/// Linux capability names (`CAP_*`, as in the OCI spec) mapped to their bit
/// index in the 64-bit capability mask.
const CAP_NAMES: &[(&str, u32)] = &[
    ("CAP_CHOWN", 0),
    ("CAP_DAC_OVERRIDE", 1),
    ("CAP_DAC_READ_SEARCH", 2),
    ("CAP_FOWNER", 3),
    ("CAP_FSETID", 4),
    ("CAP_KILL", 5),
    ("CAP_SETGID", 6),
    ("CAP_SETUID", 7),
    ("CAP_SETPCAP", 8),
    ("CAP_LINUX_IMMUTABLE", 9),
    ("CAP_NET_BIND_SERVICE", 10),
    ("CAP_NET_BROADCAST", 11),
    ("CAP_NET_ADMIN", 12),
    ("CAP_NET_RAW", 13),
    ("CAP_IPC_LOCK", 14),
    ("CAP_IPC_OWNER", 15),
    ("CAP_SYS_MODULE", 16),
    ("CAP_SYS_RAWIO", 17),
    ("CAP_SYS_CHROOT", 18),
    ("CAP_SYS_PTRACE", 19),
    ("CAP_SYS_PACCT", 20),
    ("CAP_SYS_ADMIN", 21),
    ("CAP_SYS_BOOT", 22),
    ("CAP_SYS_NICE", 23),
    ("CAP_SYS_RESOURCE", 24),
    ("CAP_SYS_TIME", 25),
    ("CAP_SYS_TTY_CONFIG", 26),
    ("CAP_MKNOD", 27),
    ("CAP_LEASE", 28),
    ("CAP_AUDIT_WRITE", 29),
    ("CAP_AUDIT_CONTROL", 30),
    ("CAP_SETFCAP", 31),
    ("CAP_MAC_OVERRIDE", 32),
    ("CAP_MAC_ADMIN", 33),
    ("CAP_SYSLOG", 34),
    ("CAP_WAKE_ALARM", 35),
    ("CAP_BLOCK_SUSPEND", 36),
    ("CAP_AUDIT_READ", 37),
    ("CAP_PERFMON", 38),
    ("CAP_BPF", 39),
    ("CAP_CHECKPOINT_RESTORE", 40),
];

/// Highest capability bit this runtime knows about (matches current kernels).
const MAX_CAP: u32 = 40;

/// `_LINUX_CAPABILITY_VERSION_3`: the 64-bit capset ABI with two data slots.
const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

/// `__NR_capset` on x86_64.
const SYS_CAPSET: i64 = 126;

#[repr(C)]
struct CapUserHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
struct CapUserData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// Resolve a single `CAP_*` name to its bit index.
fn cap_index(name: &str) -> Option<u32> {
    CAP_NAMES
        .iter()
        .find_map(|(n, idx)| (*n == name).then_some(*idx))
}

/// Build the 64-bit mask for a capability set, from the config's `CAP_*` names.
fn cap_mask(names: &[String]) -> Result<u64, Errno> {
    let mut mask = 0u64;
    for name in names {
        let idx = cap_index(name).ok_or(Errno::EINVAL)?;
        mask |= 1u64 << idx;
    }
    Ok(mask)
}

/// Set the three traditional capability sets (effective/permitted/inheritable)
/// via the `capset(2)` syscall.
fn set_cap_sets(effective: u64, permitted: u64, inheritable: u64) -> Result<(), Errno> {
    let header = CapUserHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data = [
        CapUserData {
            effective: effective as u32,
            permitted: permitted as u32,
            inheritable: inheritable as u32,
        },
        CapUserData {
            effective: (effective >> 32) as u32,
            permitted: (permitted >> 32) as u32,
            inheritable: (inheritable >> 32) as u32,
        },
    ];
    let ret = unsafe { libc::syscall(SYS_CAPSET, &header, data.as_mut_ptr()) };
    if ret == 0 { Ok(()) } else { Err(Errno::last()) }
}

/// Apply `process.capabilities` to the current process.
///
/// Runs inside the container, in its user namespace. The bounding set can only
/// be shrunk (the caller holds no privilege in the parent namespace), so it is
/// dropped bit by bit; the ambient set is raised after the traditional sets so
/// the "in permitted and inheritable" precondition holds. A set missing from
/// the config is treated as empty (secure by default).
pub fn apply_capabilities(caps: &Capabilities) -> Result<(), Errno> {
    let bounding = caps
        .bounding
        .as_deref()
        .map(cap_mask)
        .transpose()?
        .unwrap_or(0);
    let effective = caps
        .effective
        .as_deref()
        .map(cap_mask)
        .transpose()?
        .unwrap_or(0);
    let permitted = caps
        .permitted
        .as_deref()
        .map(cap_mask)
        .transpose()?
        .unwrap_or(0);
    let inheritable = caps
        .inheritable
        .as_deref()
        .map(cap_mask)
        .transpose()?
        .unwrap_or(0);
    let ambient = caps
        .ambient
        .as_deref()
        .map(cap_mask)
        .transpose()?
        .unwrap_or(0);

    // Shrink the bounding set first: PR_CAPBSET_DROP needs CAP_SETPCAP in the
    // effective set, which the capset below may remove.
    for idx in 0..=MAX_CAP {
        if bounding & (1u64 << idx) == 0 {
            let ret = unsafe { libc::prctl(libc::PR_CAPBSET_DROP, idx as libc::c_ulong, 0, 0, 0) };
            if ret != 0 {
                let err = Errno::last();
                if err != Errno::EINVAL {
                    return Err(err);
                }
            }
        }
    }

    // The kernel clears the ambient set whenever the effective or inheritable
    // sets change, so start from a clean slate and raise only what is wanted.
    unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL,
            0,
            0,
            0,
        );
    }

    set_cap_sets(effective, permitted, inheritable)?;

    for idx in 0..=MAX_CAP {
        if ambient & (1u64 << idx) != 0
            && permitted & (1u64 << idx) != 0
            && inheritable & (1u64 << idx) != 0
        {
            let ret = unsafe {
                libc::prctl(
                    libc::PR_CAP_AMBIENT,
                    libc::PR_CAP_AMBIENT_RAISE,
                    idx as libc::c_ulong,
                    0,
                    0,
                )
            };
            if ret != 0 {
                return Err(Errno::last());
            }
        }
    }

    Ok(())
}

/// Write `process.oomScoreAdj` to `/proc/self/oom_score_adj`.
pub fn apply_oom_score_adj(adj: i32) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open("/proc/self/oom_score_adj")?;
    writeln!(f, "{adj}")
}
