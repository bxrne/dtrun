# dtrun

Deterministic execution for Linux Containers (LXC).

`dtrun` is an experimental LXC-style container runtime. Like `lxc`/`runc` it
drives the kernel's own facilities — namespaces, chroot, cgroups, and ptrace —
to build a container directly from an OCI bundle, with no Docker daemon or
libcontainer in the middle. Its distinguishing feature is determinism: every
source of nondeterminism the kernel exposes to the workload is trapped and
replaced with seed-driven, reproducible values.

It is intended to provide the deterministic execution backend for
[dstest](https://github.com/bxrne/dstest), which can deterministically generate
a workload and fault scenario, but whose containers still execute under a
normal Linux environment. `dtrun` explores the other half of the problem:
making execution itself reproducible.

## Why?

Normal container execution leaves a large amount of behaviour to the host:

* thread scheduling
* wall-clock time
* randomness
* network ordering
* I/O completion
* resource availability

```text
                 dstest
                    │
          workload + faults
                    │
              ┌─────┴─────┐
              │            │
           Docker        dtrun
              │            │
        real execution   deterministic
                         execution
```

The goal is not to require applications to be rewritten for simulation. An OCI
image (and its bundle) remains the unit of deployment; only the runtime beneath
it changes.

## Usage

```sh
cargo run -- create demo --bundle examples/bundle
```

A minimal example bundle lives in `examples/bundle`. All runtime and container
output is emitted as JSON over stdout/stderr through `tracing_subscriber`; set
`RUST_LOG` to control verbosity (e.g. `RUST_LOG=debug`).

Network modes:

```sh
# isolated: own netns, loopback only (default)
cargo run -- run demo --bundle examples/bundle --net none

# shared: bind and be reached on the host network
cargo run -- run demo --bundle examples/bundle --net host
```

## Conformance

`dtrun` validates its OCI compliance inside the container using
[runtime-tools](https://github.com/opencontainers/runtime-tools)' `runtimetest`.
Current result: **94/94 TAP tests pass** (61 pass, 33 skipped as
not-configured). Every default-device, filesystem, namespace, process, and
OCI-state check passes.

The 33 skipped tests are skipped by `runtimetest` itself because the bundle
does not configure the optional feature they exercise — they are "MAY"
provisions of the spec, not runtime defects. They fall into two groups:

- **12 unset optional spec fields.** `process.capabilities`,
  `process.oomScoreAdj`, `process.ApparmorProfile`, `linux.seccomp`,
  `linux.sysctl`, `linux.uidMappings`, `linux.gidMappings`,
  `linux.maskedPaths`, `linux.readonlyPaths`, `linux.rootfsPropagation`,
  `linux.mountlabel`, and `linux.devices`. `runtimetest` reports `# SKIP` when
  a field is absent (e.g. `linux.seccomp not set`) rather than testing an
  empty value. Adding any of these to a bundle makes the corresponding test
  run — and fail until dtrun implements that feature.

- **21 default-device metadata checks.** Each of the seven default devices
  (`/dev/null`, `/dev/zero`, `/dev/full`, `/dev/random`, `/dev/urandom`,
  `/dev/tty`, `/dev/ptmx`) has three checks — permissions, user ID, and group
  ID — that `runtimetest` skips with "unconfigured" when the bundle declares no
  `FileMode`/`UID`/`GID` for the device. The devices themselves are present and
  correct (type, major, minor all pass); only their per-device metadata
  configuration is untested.

## Tests

Unit and integration tests cover the OCI config parser and the runtime's
determinism guarantees (byte-identical traces for a fixed seed, seed-dependent
getrandom bytes, exit-code and stdout propagation, and the create/start
lifecycle). A multithreaded test compiles a static pthread workload (four
workers contending on a mutex around a `getrandom`) and asserts its
thread-interleaved trace is byte-identical across runs with the same seed; it
is skipped when no C compiler or static pthread build is available:

```sh
cargo test
```

### Preparing the test rootfs

The rootfs directories (`examples/bundle/rootfs`, `examples/httpbin/rootfs`)
are gitignored built artifacts, so a fresh checkout has none and the tests
fail with `rootfs ... does not exist`. Rebuild the busybox rootfs the tests
(and `examples/bundle/config.json`) depend on with:

```sh
# from a static busybox ($BUSYBOX_BIN, busybox on PATH, or busybox-static)
scripts/build-busybox-rootfs.sh examples/bundle/rootfs
```

The script prefers an explicit static busybox binary, otherwise any busybox on
the PATH, then the `busybox-static` package, and finally falls back to
exporting the `busybox` docker image (which is what CI does after
`apt-get install busybox-static`). The httpbin rootfs is built from its image
tarball — see the [httpbin example](#examples).

### Running runtimetest

Install the conformance binary with:

```sh
go install github.com/opencontainers/runtime-tools/cmd/runtimetest@master
```

The suite is skipped when `runtimetest` is not on the path. Set
`RUNTIMETEST_BIN` to point at a specific binary if it is not in `~/go/bin` or
`$GOBIN`.

## Examples

### Minimal busybox bundle

`examples/bundle` is a small busybox rootfs exercising the deterministic
syscall traps. The config runs a short shell command that prints hostname,
cwd, uid, gid, and an environment variable.

### httpbin

`examples/httpbin` runs [httpbin](https://httpbin.org) (Python/Flask via
gunicorn) inside dtrun. The rootfs is built from the `kennethreitz/httpbin`
image:

```sh
# one-time build (image tarball via docker export)
docker create --name src kennethreitz/httpbin
docker export src -o /tmp/httpbin.tar && docker rm src
cargo run -- flatten /tmp/httpbin.tar examples/httpbin/rootfs
```

Run it with host networking (background server; use `create`/`start` or a
shell `&`):

```sh
cargo run -- run httpbin --bundle examples/httpbin --net host \
  --root /tmp/httpbin-state --seed 42 --pid-file /tmp/httpbin.pid
```

httpbin listens on `0.0.0.0:8080` (a high port — an unprivileged user
namespace cannot bind ports <1024). Query it from the host:

```sh
curl http://127.0.0.1:8080/get
curl http://127.0.0.1:8080/uuid
```

Every `getrandom` call httpbin and gunicorn make is intercepted and replaced
with seed-generated bytes; the injected values are recorded in
`/tmp/httpbin-state/httpbin/trace.jsonl`. Re-running with the same `--seed`
yields byte-identical traces, while a different seed changes the injected
randomness.
