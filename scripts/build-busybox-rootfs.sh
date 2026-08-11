#!/usr/bin/env bash
# Build examples/bundle/rootfs from a static busybox binary.
#
# The rootfs is gitignored (built artifacts are not committed), so a fresh
# checkout has no rootfs. This script reconstructs a minimal one that the
# integration tests (and examples/bundle/config.json) depend on.
#
# Requirements: a static busybox binary. Sources, in order of preference:
#   $BUSYBOX_BIN              explicit path
#   command -v busybox        on PATH (must be static)
#   /usr/bin/busybox          busybox-static package (Debian/Ubuntu)
#   docker busybox image      exported via docker
#
# Usage: scripts/build-busybox-rootfs.sh [output-dir]
set -euo pipefail

ROOTFS="${1:-examples/bundle/rootfs}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

busybox_bin="${BUSYBOX_BIN:-}"
if [[ -z "$busybox_bin" ]] && command -v busybox >/dev/null 2>&1; then
    busybox_bin="$(command -v busybox)"
fi
if [[ -z "$busybox_bin" ]] && [[ -x /usr/bin/busybox ]]; then
    busybox_bin=/usr/bin/busybox
fi

# Docker fallback: export the busybox image as the rootfs verbatim.
if [[ -z "$busybox_bin" ]] && command -v docker >/dev/null 2>&1; then
    id="$(docker create busybox 2>/dev/null)"
    if [[ -n "$id" ]]; then
        mkdir -p "$TMP/root"
        docker export "$id" | tar -x -C "$TMP/root"
        docker rm "$id" >/dev/null 2>&1 || true
        rm -rf "$ROOTFS"
        mv "$TMP/root" "$ROOTFS"
        echo "built $ROOTFS from docker busybox image"
        exit 0
    fi
fi

if [[ -z "$busybox_bin" ]]; then
    echo "error: no static busybox found. Install busybox-static or set BUSYBOX_BIN" >&2
    exit 1
fi

# Verify it is static: a dynamically-linked busybox would need libc in the
# chroot, which the minimal rootfs does not provide.
if ! file "$busybox_bin" | grep -q "statically linked"; then
    echo "error: $busybox_bin is not statically linked (install busybox-static)" >&2
    exit 1
fi

rm -rf "$ROOTFS"
mkdir -p "$ROOTFS"/{bin,dev/pts,dev/shm,etc/network,home,proc,root,sys,tmp,usr/bin,usr/sbin,var/spool,var/www}
cp "$busybox_bin" "$ROOTFS/bin/busybox"

# Symlink every applet (sh, true, false, echo, hostname, pwd, id, ...).
"$ROOTFS/bin/busybox" --list |
    sed "s|^|$ROOTFS/bin/|" |
    xargs -I{} ln -sf busybox {} 2>/dev/null || true

echo "built $ROOTFS from $busybox_bin"
