# dtrun

> **Deterministic, isolated execution runtime for Linux containers.**

`dtrun` is a lightweight, OCI-compliant container runtime engineered for **reproducible, byte-identical container execution**. It isolates workloads using native Linux namespaces (`user`, `net`, `mount`, `pid`, `ipc`, `uts`), `chroot`, and `cgroups v2`, while using a `ptrace` supervisor to trap and neutralize all sources of host kernel nondeterminism (clocks, randomness, ASLR, and thread scheduling).

`dtrun` serves as the execution engine for [dstest](https://github.com/bxrne/dstest) and can be used as a standalone CLI or embedded as a Rust library (`libdtrun`).


## Key Features

* **100% Deterministic Execution**: Seed-driven execution (`--seed`) guarantees reproducible execution across runs. Same seed, same execution trajectory.
* **Full-Stack Container Isolation**:
  * **Network**: Isolated network namespace (`CLONE_NEWNET`) with loopback-only default (`--net none`) to block unhandled host network leaks.
  * **Storage & Mounts**: Private mount namespace (`CLONE_NEWNS`), `chroot` rootfs jailing, read-only root options, and path masking.
  * **User / Security**: Unprivileged user namespace (`CLONE_NEWUSER`) mapping container `root` safely to an unprivileged host user.
  * **Process & IPC**: Isolated PID (`CLONE_NEWPID`), IPC (`CLONE_NEWIPC`), and UTS (`CLONE_NEWUTS`) namespaces.
  * **Resource Control**: Best-effort Cgroups v2 limits (`memory.max`, `cpu.max`, `pids.max`).
* **Virtual Clock & Randomness Interception**: Traps `clock_gettime`, `gettimeofday`, and `getrandom` via `ptrace`, replacing non-deterministic values with reproducible, seed-based data.
* **Deterministic Thread Scheduling**: Serializes thread execution in a FIFO round-robin order with custom `futex` wait parking to eliminate race conditions.
* **JSONL Execution Tracing**: Records every intercepted syscall, time jump, random byte injection, thread switch, and output line into a replayable JSON Lines trace file.
* **OCI Bundle & Image Support**: Runs standard OCI bundles (`config.json` + `rootfs`) and includes `dtrun flatten` to convert Docker/OCI image tarballs into normalized, deterministic root filesystems.


## Isolation Overview

| Subsystem | Isolation Mechanism | Default Behavior |
| :--- | :--- | :--- |
| **Network** | Network Namespace (`CLONE_NEWNET`) | Isolated loopback-only (`none`). Optional `--net host`. |
| **Filesystem** | Mount Namespace (`CLONE_NEWNS`) + `chroot` | Rootfs jailed, private mounts, masked & read-only paths. |
| **User / Security** | User Namespace (`CLONE_NEWUSER`) | Container root (UID 0) mapped to unprivileged host UID. |
| **Processes** | PID Namespace (`CLONE_NEWPID`) | Workload starts as PID 1; host processes invisible. |
| **IPC & UTS** | IPC (`CLONE_NEWIPC`) & UTS (`CLONE_NEWUTS`) | Isolated System V IPC queues and hostname (`sethostname`). |
| **Resources** | Cgroups v2 | Applies CPU, memory, swap, and PID limits if delegated. |


## Architecture

* **`libdtrun`**: The core Rust library providing OCI parsing, namespace setup, image flattening, state management, and the `ptrace` determinism engine.
* **`dtrun`**: An OCI-compliant command-line tool wrapping `libdtrun`.


## Prerequisites

* **Linux Kernel** (6.0+ recommended).
* **Rust** stable toolchain.
* **Unprivileged User Namespaces** enabled on your host.
  * *Note for Ubuntu 24.04+*: If restricted by AppArmor, run:
    ```sh
    sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
    ```
* **Static busybox binary** (e.g., `apt install busybox-static`) for testing example bundles.


## Quick Start

### 1. Build `dtrun`

```sh
cargo build --release
```

### 2. Flatten an Image or Use the Demo Bundle

Generate a default OCI specification:
```sh
./target/release/dtrun spec my-bundle
```

Or run the built-in demo container (after setting up `examples/bundle/rootfs` as described in [DOCS.md](DOCS.md)):
```sh
cargo run -- run demo --bundle examples/bundle --seed 42
```

All CLI output and state changes follow standard OCI conventions. Verbose logging can be enabled via `RUST_LOG=debug`.


## Testing & Conformance

Run the test suite:
```sh
cargo test
```

`dtrun` can also be evaluated against the OCI runtime spec test suite (`runtimetest` from `opencontainers/runtime-tools`):
```sh
dtrun run my-container --bundle /path/to/bundle
```


## Documentation

For full details on the CLI, library Rust API (`libdtrun`), execution trace schema, image flattening, and roadmap, consult **[DOCS.md](DOCS.md)**.

