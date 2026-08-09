//! Network isolation: each container runs in its own network namespace with
//! only the loopback interface brought up. The interface state is fixed, so no
//! host-side network ordering or addressing leaks into the workload.

use nix::errno::Errno;

/// Errors produced while configuring the container network namespace.
#[derive(Debug)]
pub enum NetError {
    Socket(Errno),
    Ioctl(Errno),
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetError::Socket(e) => write!(f, "socket failed: {e}"),
            NetError::Ioctl(e) => write!(f, "ioctl failed: {e}"),
        }
    }
}

impl std::error::Error for NetError {}

/// Mirror of the kernel's `struct ifreq` for the flag ioctls. Only `ifr_name`
/// and `ifr_flags` are touched by `SIOCGIFFLAGS`/`SIOCSIFFLAGS`.
#[repr(C)]
struct IfReq {
    name: [u8; 16],
    flags: libc::c_short,
    _pad: [u8; 22],
}

const IFNAMSIZ: usize = 16;

/// Bring the loopback interface up inside the container's network namespace.
///
/// Must run after entering the new user + network namespaces, where the
/// caller holds CAP_NET_ADMIN for that network namespace.
pub fn bring_up_loopback() -> Result<(), NetError> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(NetError::Socket(Errno::last()));
    }

    let mut name = [0u8; IFNAMSIZ];
    name[..2].copy_from_slice(b"lo");
    let mut ifr = IfReq {
        name,
        flags: 0,
        _pad: [0u8; 22],
    };

    let r = unsafe { libc::ioctl(fd, libc::SIOCGIFFLAGS, &mut ifr as *mut IfReq) };
    if r < 0 {
        unsafe { libc::close(fd) };
        return Err(NetError::Ioctl(Errno::last()));
    }

    ifr.flags |= libc::IFF_UP as libc::c_short;

    let r = unsafe { libc::ioctl(fd, libc::SIOCSIFFLAGS, &ifr as *const IfReq) };
    unsafe { libc::close(fd) };
    if r < 0 {
        return Err(NetError::Ioctl(Errno::last()));
    }
    Ok(())
}
