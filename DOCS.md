# dtrun Reference Documentation

* [Overview](#overview)
* [Build and install](#build-and-install)
* [Architecture](#architecture)
* [Concepts](#concepts)
* [The OCI bundle](#the-oci-bundle)
* [Command-line reference](#command-line-reference)
* [Library usage](#library-usage)
* [Networking](#networking)
* [Mounts and devices](#mounts-and-devices)
* [Cgroups](#cgroups)
* [State and lifecycle](#state-and-lifecycle)
* [Execution trace format](#execution-trace-format)
* [Image flattening](#image-flattening)
* [Conformance](#conformance)
* [Testing](#testing)
* [Limitations](#limitations)
* [To be implemented](#to-be-implemented)

## Overview

`dtrun` is an experimental container runtime for Linux. It runs an OCI bundle
under deterministic execution.

A normal container run depends on the host. Thread scheduling, wall-clock
time, randomness, and network ordering can differ between runs. `dtrun`
removes these dependencies. It traps each source of nondeterminism at the
syscall boundary. It replaces each source with seed-driven values.

The result is byte-identical execution for a fixed seed. The runtime records
every decision in a trace file for later replay.

## Build and install

Build the release binary:

```sh
cargo build --release
```

The binary is `target/release/dtrun`. The library artifact is
`target/release/libdtrun.a` (or `.rlib`).

To run without installing, use `cargo run`.

## Architecture

The repository has two targets.

### The library, `libdtrun`

The library is the deterministic execution backend. It provides:

* OCI config parsing and validation.
* Deterministic image flattening.
* Namespace and mount setup.
* The ptrace supervisor.
* State and trace persistence.

Downstream tools can use it directly. They do not need to invoke the binary.

### The binary, `dtrun`

The binary is a thin command-line interface over the library. It follows the
OCI runtime command conventions. Each subcommand maps to a small number of
library calls.

## Concepts

### The seed

The seed is a `u64` value. It drives every deterministic decision:

* The injected `getrandom` bytes.
* The virtual clock state.
* The thread scheduling order.

Use the same seed to reproduce a run. Use a different seed to change the
injected values. The default seed is `42`.

### The virtual clock

`dtrun` replaces the kernel clock with a virtual clock.

* `CLOCK_REALTIME` returns a fixed epoch. The value is `1704067200`.
* `CLOCK_MONOTONIC` and `CLOCK_BOOTTIME` advance by a fixed step per read.
  The step is `1,000,000` nanoseconds.
* `gettimeofday` uses the same virtual clock.

A run therefore reports the same time every time, for a fixed seed.

### The ptrace supervisor

The supervisor traces every syscall of the container init process and its
children. It uses `PTRACE_SYSCALL`. It intercepts the syscalls that produce
nondeterminism:

* `getrandom`.
* `clock_gettime`.
* `gettimeofday`.

For each intercepted syscall, the supervisor writes a deterministic result
into the tracee memory and returns. It records the injection in the trace.

The supervisor also disables ASLR. It sets the `ADDR_NO_RANDOMIZE`
personality before the entrypoint execs. The address layout is therefore
fixed across runs.

### Thread scheduling

The supervisor runs threads one at a time. It switches threads only at
syscall boundaries. The order is FIFO round-robin.

A `futex` waiter is parked in the supervisor. It does not block in the
kernel. Another thread runs instead. A waiting thread can never stall the
scheduler.

The scheduling is deterministic. Same seed, same interleaving.

## The OCI bundle

An OCI bundle is a directory with two parts:

* `config.json`. The runtime configuration.
* `rootfs/`. The container root filesystem.

`dtrun` reads `config.json` and validates it. It then prepares the container.

The example bundle is in `examples/bundle`.

Generate a default `config.json`:

```sh
dtrun spec
```

The command writes the file to the current directory. Give a path to write
elsewhere:

```sh
dtrun spec /path/to/bundle
```

## Command-line reference

Run `dtrun --help` for the full option list. The global options are:

* `--root PATH`. The state root directory.
* `--seed N`. The determinism seed. The default is `42`.
* `--log FILE`. Write runtime logs to `FILE`.

### create

Prepare a container but do not start it.

```sh
dtrun create <id> --bundle <path> [--pid-file <file>] [--net <mode>]
```

The container pauses in the `created` state. The supervisor daemon owns it.

### start

Start a container that is in the `created` state.

```sh
dtrun start <id>
```

### run

Create, start, wait for, and delete a container with one command.

```sh
dtrun run <id> --bundle <path> [--seed <n>] [--net <mode>]
```

The command returns the container exit code.

### kill

Send a signal to the container init process.

```sh
dtrun kill <id> [signal]
```

The default signal is `SIGTERM`.

### delete

Remove the container state.

```sh
dtrun delete <id> [--force]
```

Use `--force` to delete a running container. `--force` sends `SIGKILL`.

### state

Print the container state as JSON.

```sh
dtrun state <id>
```

### list

List the containers known to the runtime.

```sh
dtrun list [--format table|json]
```

The default format is `table`.

### exec

Run a command inside a running container.

```sh
dtrun exec <id> <command...> [--cwd PATH] [--env KEY=VALUE]
```

### spec

Write a default OCI `config.json` to a directory.

```sh
dtrun spec [PATH]
```

The default path is the current directory.

### flatten

Flatten an image tarball into a deterministic rootfs.

```sh
dtrun flatten <IMAGE> <DEST>
```

The image is a `.tar` or `.tar.gz` file.

### version

Print the version information.

```sh
dtrun version
```

## Library usage

Add the library to a Cargo project:

```toml
[dependencies]
libdtrun = { path = "../dtrun" }
```

Load a config and run a container:

```rust
use std::path::Path;
use libdtrun::OciConfig;
use libdtrun::runtime::{Host, NetMode};

let bundle = Path::new("/path/to/bundle");
let config = OciConfig::from_path(&bundle.join("config.json"))?;
let host = Host::with_net(config, bundle.to_path_buf(), 42, NetMode::None);
let code = host.run(&Path::new("/tmp/dtrun-state"), "my-container")?;
```

The public API surface is:

* `libdtrun::OciConfig`. The parsed and validated bundle config.
* `libdtrun::runtime::Host`. The runtime host.
* `libdtrun::runtime::NetMode`. The network mode.
* `libdtrun::runtime::state`. State and trace persistence.
* `libdtrun::oci::image`. Deterministic image flattening.

## Networking

Each container runs in its own network namespace. Only the loopback interface
is brought up. The interface state is fixed. Host-side network ordering does
not leak into the workload.

Two network modes exist:

* `none`. The container has its own network namespace with loopback only.
  This is the default.
* `host`. The container shares the host network namespace. It can bind and be
  reached on the host addresses.

Set the mode with the `--net` option.

## Mounts and devices

`dtrun` applies the mounts from `config.json` after `chroot`. The example
bundle declares:

* `/proc` as `proc`.
* `/dev` as `tmpfs`.
* `/dev/pts` as `devpts`.
* `/sys` as `sysfs` (read-only).
* `/dev/mqueue` as `mqueue`.
* `/dev/shm` as `tmpfs`.

`dtrun` also creates the OCI default devices:

* `/dev/null`.
* `/dev/zero`.
* `/dev/full`.
* `/dev/random`.
* `/dev/urandom`.
* `/dev/tty`.

The host device nodes are opened before `chroot`. They are bind-mounted into
the container. `mknod` is forbidden inside a user namespace, so this method
avoids it.

The runtime remounts the root mount as private. This stops host mount
propagation from leaking into the container mount namespace.

## Cgroups

`dtrun` applies cgroup v2 resource limits when the host delegates a writable
cgroup tree. It reads the OCI `linux.resources` block and writes:

* `pids.max`.
* `memory.max`.
* `memory.swap.max`.
* `cpu.max`.
* `cpu.weight`.

The limits are best-effort. On hosts without delegation, the container runs
without limits. The container never fails to start because cgroup delegation
is missing.

## State and lifecycle

The state root stores one directory per container. The default state root is:

* `$XDG_RUNTIME_DIR/dtrun`.
* Else `/run/dtrun`, if writable.
* Else a uid-scoped temporary directory.

A container directory contains:

* `state.json`. The OCI state object.
* `trace.jsonl`. The execution trace.
* `exec.fifo`. The FIFO used to separate `create` from `start`.

The lifecycle statuses are:

* `creating`.
* `created`.
* `running`.
* `stopped`.

The `create` command prepares the container and pauses it in the `created`
state. The `start` command writes one byte into the FIFO. The supervisor then
releases the container.

## Execution trace format

The trace is a JSON Lines file. Each line is one event. The event record is:

```json
{"seq": 1, "event": "getrandom", "detail": {"nbytes": 8, "bytes": "1f2e..."}}
```

The fields are:

* `seq`. A monotonic sequence number.
* `event`. The event name.
* `detail`. The event-specific data.

The event names include:

* `getrandom`. An injected random read. `detail.bytes` is the hex value.
* `clock_gettime`. A virtual clock read. `detail` has `clock`, `sec`, `nsec`.
* `gettimeofday`. A virtual time read. `detail` has `sec`, `usec`.
* `schedule`. A thread switch. `detail` has `from` and `to`.
* `thread_create`. A new thread. `detail` has the thread id.
* `thread_exit`. A finished thread. `detail` has the thread id.
* `futex`. A futex operation. `detail` has `op`, `kind`, and `tid`.
* `exit`. The container exit. `detail` has `code`.
* `stdout`. A captured stdout line. `detail` has `line`.
* `stderr`. A captured stderr line. `detail` has `line`.

## Image flattening

`dtrun` flattens an OCI or Docker image tarball into a rootfs. The process
normalizes the metadata:

* Every file gets mtime `0`.
* Every file gets a canonical mode.
* Ownership is `0:0` where permitted.
* Docker whiteout semantics are honoured.

The result is a deterministic rootfs. The command is:

```sh
dtrun flatten image.tar.gz dest/
```

## Conformance

`dtrun` validates its OCI compliance inside the container using
`runtimetest` from `opencontainers/runtime-tools`.

Current result: **349/349 TAP tests pass**. 312 tests pass and 37 are skipped.

The skipped tests fall into two groups:

* Rootless/user-namespace limits that no `config.json` can satisfy:
  * The 21 default-device permission/uid/gid checks are hardcoded in
    `runtimetest` (its bundled device list never sets those fields).
  * The 12 `linux.devices` uid/gid checks cannot pass because device nodes are
    bind-mounted from the host (mknod is forbidden inside a user namespace), so
    their uid is the host's unmapped root.
* Features dtrun does not implement yet: `linux.seccomp`,
  `linux.rootfsPropagation`, `linux.mountLabel`, and `process.apparmorProfile`.
  The first three have no meaningful pass outcome even when configured in a
  rootless runtime (`runtimetest` skips the seccomp check by design, and the
  propagation check needs to bind-mount the root).

## Testing

Run the full suite:

```sh
cargo test
```

### Prerequisites

The suite needs:

* A static busybox binary. Install `busybox-static` on Debian/Ubuntu. The
  binary must be statically linked, because the minimal rootfs has no dynamic
  loader.
* An environment that supports unprivileged user namespaces and mount
  operations.

Ubuntu 24.04 and later restrict unprivileged user namespaces by default. Lift
the restriction before the runtime tests:

```sh
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
```

### Build the test rootfs

The rootfs directories (`examples/bundle/rootfs`, `examples/httpbin/rootfs`)
are gitignored built artifacts. A fresh checkout has none, and the tests fail
with `rootfs ... does not exist`. Rebuild the busybox rootfs that the tests
and `examples/bundle/config.json` depend on:

```sh
ROOTFS=examples/bundle/rootfs
rm -rf "$ROOTFS"
mkdir -p "$ROOTFS"/{bin,dev/pts,dev/shm,etc/network,home,proc,root,sys,tmp,usr/bin,usr/sbin,var/spool,var/www}
cp /bin/busybox "$ROOTFS/bin/busybox"
"$ROOTFS/bin/busybox" --list | grep -v '^busybox$' | \
  sed "s|^|$ROOTFS/bin/|" | xargs -I{} ln -sf busybox {} 2>/dev/null || true
```

Create one symlink per applet (`sh`, `true`, `echo`, and so on). Do not create
a symlink for the `busybox` applet itself. `--list` includes it, and replacing
the binary with a self-referential symlink makes `exec` return `ELOOP`.

The `examples/httpbin/rootfs` is built from an image tarball with `dtrun
flatten` (see the httpbin example in the README).

### Run runtimetest

To validate OCI compliance using `runtimetest` (from `opencontainers/runtime-tools`):

```sh
go install github.com/opencontainers/runtime-tools/cmd/runtimetest@master
cp $(which runtimetest) examples/bundle/rootfs/runtimetest
# update config.json process.args to ["/runtimetest"]
dtrun run test-conformance --bundle examples/bundle
```

## Limitations

* The runtime needs an environment that permits unprivileged user namespaces.
* Cgroup limits are best-effort. They depend on host delegation.
* The root filesystem must be accessible to the caller.
* Mount operations are limited to the filesystem types permitted inside a
  user namespace.

## To be implemented

The following OCI runtime-spec features are not implemented yet:

* Seccomp filters (`linux.seccomp`).
* AppArmor and SELinux profiles (`process.apparmorProfile`,
  `linux.mountLabel`).
* Rootfs propagation configuration (`linux.rootfsPropagation`).
* OCI lifecycle hooks.
* Checkpoint and restore.
