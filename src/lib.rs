//! libdtrun: OCI config parsing and deterministic container runtime primitives.
//!
//! This crate ships a library and a `dtrun` CLI binary. The library is the
//! deterministic execution backend. It loads an OCI bundle, supervises a
//! container under a seeded ptrace scheduler, and records a replayable trace.
//! Downstream projects (such as dstest) can drive it programmatically without
//! going through the CLI.
//!
//! # Minimal example
//!
//! ```no_run
//! use std::path::Path;
//! use libdtrun::OciConfig;
//! use libdtrun::runtime::{Host, NetMode};
//!
//! let bundle = Path::new("/path/to/bundle");
//! let config = OciConfig::from_path(&bundle.join("config.json"))?;
//! let host = Host::with_net(config, bundle.to_path_buf(), 42, NetMode::None);
//! let code = host.run(&Path::new("/tmp/dtrun-state"), "my-container")?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod cli;
pub mod oci;
pub mod runtime;

pub use oci::config::OciConfig;
pub use runtime::{Host, NetMode};
