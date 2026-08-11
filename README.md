# dtrun

Deterministic execution for Linux containers (LXC).

`dtrun` is an experimental LXC-style container runtime. It builds a container
directly from an OCI bundle using namespaces, chroot, cgroups, and ptrace. Its
main feature is determinism: each source of nondeterminism the kernel exposes
to a workload is trapped and replaced with a seed-driven, reproducible value.

## Architecture

* `libdtrun` - the deterministic execution backend (library).
* `dtrun` - a thin command-line interface over the library.

`dtrun` provides the deterministic execution backend for
[dstest](https://github.com/bxrne/dstest). See [DOCS.md](DOCS.md) for the full
reference: CLI, library API, trace format, and roadmap.

## Prerequisites

* Rust stable toolchain.
* A static busybox binary (install `busybox-static` on Debian/Ubuntu).
* Unprivileged user namespaces and mount privileges.

## Quick start

Build the test rootfs from a static busybox, then run the example container:

```sh
cargo build
# build examples/bundle/rootfs (steps in DOCS.md, section Testing)
cargo run -- run demo --bundle examples/bundle
```

All runtime and container output is JSON over stdout/stderr. Set `RUST_LOG`
to control verbosity (for example, `RUST_LOG=debug`).

## Tests

```sh
cargo test
```

Ubuntu 24.04 and later restrict unprivileged user namespaces by default. Lift
the restriction before the runtime tests:

```sh
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
```

## Conformance

`dtrun` passes **349/349** `runtimetest` TAP checks (312 pass, 37 skipped). The
skips are either hardcoded in `runtimetest` (default-device permissions/uid/gid)
or unreachable rootless (`linux.devices` uid/gid, seccomp, rootfs propagation,
mount labels, AppArmor). Run the suite yourself with

```sh
dtrun conformance --bundle examples/bundle
```

## Examples

* `examples/bundle` - minimal busybox container.
* `examples/httpbin` - httpbin under deterministic execution.

See [DOCS.md](DOCS.md) for the full CLI reference and the to-be-implemented
roadmap.
