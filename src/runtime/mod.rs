//! Container runtime: turns an [`OciConfig`] into an isolated, deterministically
//! supervised child and implements the OCI lifecycle.

pub mod cgroup;
pub mod child;
pub mod credentials;
pub mod exec;
pub mod host;
pub mod mounts;
pub mod namespaces;
pub mod net;
pub mod ops;
pub mod signal;
pub mod state;
pub mod supervisor;

pub use exec::exec_in_container;
pub use host::{Host, NetMode};
pub use ops::{delete_container, kill_container, list_containers, print_state, start_container};
