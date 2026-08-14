# dtrun

[![Crates.io](https://img.shields.io/crates/v/dtrun.svg)](https://crates.io/crates/dtrun)
[![Documentation](https://docs.rs/dtrun/badge.svg)](https://docs.rs/dtrun)
[![CI](https://github.com/bxrne/dtrun/actions/workflows/ci.yml/badge.svg)](https://github.com/bxrne/dtrun/actions)
![Tag](https://img.shields.io/github/v/tag/bxrne/dtrun?include_prereleases&sort=semver&style=flat)
[![Rust Edition](https://img.shields.io/badge/rustc-2024-orange.svg)](Cargo.toml)

> **Deterministic, isolated execution runtime for Linux containers.**

`dtrun` is a lightweight, OCI-compliant container runtime engineered for **reproducible, byte-identical container execution** on Linux. It isolates workloads using native Linux kernel namespaces (`user`, `net`, `mount`, `pid`, `ipc`, `uts`), `chroot` rootfs jailing, and `cgroups v2`, while employing a `ptrace` supervisor to trap and neutralize all sources of host kernel nondeterminism (clocks, randomness, thread scheduling, and memory layout).

`dtrun` serves as the execution engine for **[dstest](https://github.com/bxrne/dstest)** and can be used either as a standalone CLI binary (`dtrun`) or embedded directly into Rust applications via `libdtrun`.


## Key Features

- **100% Deterministic Execution**: Seed-driven execution (`--seed`) guarantees reproducible container execution across runs. The exact same seed yields the identical execution trajectory every single time.
- **Full-Stack Container Isolation**:
  - **Network**: Private network namespace (`CLONE_NEWNET`) with loopback-only default (`--net none`) or host network access (`--net host`).
  - **Storage & Mounts**: Private mount namespace (`CLONE_NEWNS`), `chroot` rootfs jailing, read-only root filesystem options, masked paths (`/proc/kcore`, `/sys/firmware`), and standard Linux pseudofs (`/proc`, `/sys`, `/dev`, `/dev/pts`, `/dev/shm`).
  - **User & Security**: Unprivileged user namespace (`CLONE_NEWUSER`) mapping container `root` (UID 0) safely to an unprivileged host UID.
  - **Process & IPC**: Isolated process trees (`CLONE_NEWPID`), System V IPC (`CLONE_NEWIPC`), and hostnames (`CLONE_NEWUTS`).
  - **Resource Limits**: Best-effort Cgroups v2 limits (`memory.max`, `cpu.max`, `pids.max`).
- **Virtual Clock & Randomness Interception**: Traps `clock_gettime`, `gettimeofday`, and `getrandom` via `ptrace`, replacing non-deterministic host values with reproducible, seed-derived data streams.
- **Deterministic Thread Scheduling**: Serializes thread execution in a FIFO round-robin order with custom `futex` wait parking to eliminate race conditions and non-deterministic concurrency.
- **JSONL Execution Tracing**: Captures every intercepted syscall, time jump, random byte injection, thread switch, and output line into a structured, replayable JSON Lines trace file.
- **OCI Bundle & Image Support**: Runs standard OCI bundles (`config.json` + `rootfs`) and includes `dtrun flatten` to convert multi-layer Docker/OCI image tarballs into normalized, deterministic root filesystems.


## Isolation Matrix

| Subsystem | Isolation Mechanism | Default Behavior |
| :--- | :--- | :--- |
| **Network** | Network Namespace (`CLONE_NEWNET`) | Isolated loopback-only (`none`). Optional `--net host`. |
| **Filesystem** | Mount Namespace (`CLONE_NEWNS`) + `chroot` | Rootfs jailed, private mounts, masked & read-only paths. |
| **User / Security** | User Namespace (`CLONE_NEWUSER`) | Container root (UID 0) mapped to unprivileged host UID. |
| **Processes** | PID Namespace (`CLONE_NEWPID`) | Workload starts as PID 1; host processes invisible. |
| **IPC & UTS** | IPC (`CLONE_NEWIPC`) & UTS (`CLONE_NEWUTS`) | Isolated System V IPC queues and hostname (`sethostname`). |
| **Resources** | Cgroups v2 | Applies CPU, memory, swap, and PID limits if delegated. |


## Installation

### From Crates.io

```sh
cargo install dtrun
```

### Adding as a Library (`libdtrun`)

Add `libdtrun` to your `Cargo.toml`:

```toml
[dependencies]
libdtrun = "0.1"
```

### From Source

```sh
git clone https://github.com/bxrne/dtrun.git
cd dtrun
cargo build --release
```

The compiled binary will be placed at `target/release/dtrun`.


## Quick Start

### 1. Generate an OCI Specification

Create a default OCI bundle configuration (`config.json`):

```sh
dtrun spec my-bundle
```

### 2. Prepare a Root Filesystem

Flatten a Docker/OCI image tarball into a normalized root filesystem:

```sh
dtrun flatten image.tar my-bundle/rootfs
```

### 3. Run a Container Deterministically

Run the container with a fixed seed:

```sh
dtrun run my-container --bundle my-bundle --seed 42
```

For verbose internal logging, set `RUST_LOG=debug`.


## CLI Reference

`dtrun` follows standard OCI container runtime CLI conventions:

| Subcommand | Description |
| :--- | :--- |
| `run` | Create, start, wait for, and clean up a container in a single command |
| `create` | Initialize container isolation and parse bundle without executing workload |
| `start` | Begin execution of a previously created container |
| `exec` | Run an additional command inside a running container |
| `kill` | Send a signal (e.g., `SIGTERM`, `SIGKILL`) to the container's init process |
| `delete` | Remove container state and associated resources |
| `state` | Output the OCI state JSON of a container |
| `list` | Display all containers managed by `dtrun` |
| `spec` | Generate a default `config.json` OCI spec file |
| `flatten` | Extract and normalize a Docker/OCI image archive into a deterministic rootfs |
| `version` | Display version and build information |


## Library Usage (`libdtrun`)

Downstream Rust projects (such as fault injection tools or simulators) can embed `libdtrun` directly to supervise containers programmatically:

```rust
use std::path::Path;
use libdtrun::OciConfig;
use libdtrun::runtime::{Host, NetMode};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bundle = Path::new("/path/to/bundle");
    let config = OciConfig::from_path(&bundle.join("config.json"))?;

    // Configure deterministic host runner with seed 42
    let host = Host::with_net(config, bundle.to_path_buf(), 42, NetMode::None);

    // Run container with state directory
    let exit_code = host.run(Path::new("/tmp/dtrun-state"), "my-container")?;
    println!("Container exited with code: {}", exit_code);

    Ok(())
}
```


## Prerequisites & System Requirements

- **Linux Kernel**: 6.0+ recommended.
- **Rust Toolchain**: 1.85+ (2024 edition).
- **Unprivileged User Namespaces**: Enabled on the host system.
  - *Ubuntu 24.04+ Note*: If AppArmor restricts unprivileged user namespaces, lift the restriction via:
    ```sh
    sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
    ```
- **Static Workload Binaries**: (e.g., `busybox-static`) when assembling minimal test bundles.


## Testing & Conformance

Run the test suite:

```sh
cargo test
```

`dtrun` can also be evaluated against the standard OCI runtime spec test suite ([`runtimetest`](https://github.com/opencontainers/runtime-tools)):

```sh
dtrun run test-container --bundle /path/to/oci-test-bundle
```


## AI Assistant Integration

This repository includes an AI skill ([`SKILL.md`](SKILL.md)) to assist AI tools (e.g., Claude Code, Opencode, Cursor, Antigravity) in operating `dtrun`.

To install the skill for your local AI assistant:

```sh
# For Claude Code / Opencode
mkdir -p ~/.config/opencode/skills/dtrun
cp SKILL.md ~/.config/opencode/skills/dtrun/SKILL.md

# For other agents (e.g., ~/.agents/skills/)
mkdir -p ~/.agents/skills/dtrun
cp SKILL.md ~/.agents/skills/dtrun/SKILL.md
```

Then prompt your assistant with: *"Use the dtrun skill to inspect or run deterministic container experiments."*


## Documentation

For full API references, architecture details, trace formats, and advanced configuration options, see **[DOCS.md](DOCS.md)**.


## License

This project is licensed under the [MIT License](LICENSE).
