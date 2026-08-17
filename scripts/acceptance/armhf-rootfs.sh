#!/bin/bash
# rathole-x ARMv7 (Raspberry Pi 3B 32-bit) emulated-rootfs acceptance test.
# Runs ON THE HOST: drives a static armv7 musl binary inside an Alpine armhf
# minirootfs via qemu-arm-static + chroot. ARMv7 hard-float is the same
# userland ISA as Raspberry Pi OS 32-bit; a real Pi rootfs can be substituted
# via ROOTFS=...
#
# Host usage (from the repo root):
#   cargo build --locked --release --target armv7-unknown-linux-musleabihf \
#     --no-default-features --features server,client,rustls,noise,websocket-rustls,hot-reload
#   scripts/acceptance/armhf-rootfs.sh
#
# Requires: qemu-user-static, sudo (for chroot), curl (first run only),
# the cross toolchain from docs/build-guide.md ("Cross-compiling and
# Testing for ARM"). Every check is fatal.
set -euo pipefail

ROOTFS=${ROOTFS:-"$HOME/Codes/rootfs/alpine-armhf"}
ALPINE_VERSION=${ALPINE_VERSION:-3.20.3}
ALPINE_MIRROR=${ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/armhf}
BIN=target/armv7-unknown-linux-musleabihf/release/rathole-x
# Dedicated ports: never collide with a real installed service.
CONTROL_PORT=25333
EXPOSED_PORT=25334
LOCAL_PORT=28080
PAYLOAD="ping-from-armv7"

fail() { echo "FAIL: $*" >&2; exit 1; }
guest() { sudo chroot "$ROOTFS" qemu-arm-static "$@"; }

cleanup() {
    sudo pkill -f 'qemu-arm-static.*armhf-(server|client)' 2>/dev/null || true
    sudo pkill -f "qemu-arm-static.*nc -lk -p $LOCAL_PORT" 2>/dev/null || true
}
trap cleanup EXIT

[ -x "$BIN" ] || fail "$BIN missing; build the armv7 musl release binary first (see header)"
file "$BIN" | grep -q 'ARM.*statically linked' \
    || fail "$BIN is not a static ARM binary: $(file "$BIN")"

echo "=== prepare armhf rootfs at $ROOTFS ==="
if [ ! -x "$ROOTFS/bin/busybox" ]; then
    mkdir -p "$ROOTFS"
    curl -sSL "$ALPINE_MIRROR/alpine-minirootfs-$ALPINE_VERSION-armhf.tar.gz" \
        | tar xzf - -C "$ROOTFS"
fi
[ -x "$ROOTFS/bin/busybox" ] || fail "rootfs at $ROOTFS has no busybox"
cp /usr/bin/qemu-arm-static "$ROOTFS/usr/bin/"
cp "$BIN" "$ROOTFS/usr/local/bin/rathole-x"
mkdir -p "$ROOTFS/etc/rathole-x"
cat > "$ROOTFS/etc/rathole-x/armhf-server.toml" <<EOF
[server]
bind_addr = "127.0.0.1:$CONTROL_PORT"

[server.services.demo]
type = "tcp"
token = "armhf-acceptance"
bind_addr = "127.0.0.1:$EXPOSED_PORT"
EOF
cat > "$ROOTFS/etc/rathole-x/armhf-client.toml" <<EOF
[client]
remote_addr = "127.0.0.1:$CONTROL_PORT"

[client.services.demo]
type = "tcp"
token = "armhf-acceptance"
local_addr = "127.0.0.1:$LOCAL_PORT"
EOF
guest /bin/busybox uname -m | grep -q armv7l || fail "rootfs is not armv7l"
echo "rootfs ready: Alpine $(guest /bin/busybox cat /etc/alpine-release), $(guest /bin/busybox uname -m)"

echo "=== start echo backend, server, client inside the emulated rootfs ==="
# nc must be invoked as a plain ELF: qemu-arm-static cannot exec shebang
# scripts (they fail with "Invalid ELF image").
guest /bin/busybox nc -lk -p "$LOCAL_PORT" -e /bin/cat >/dev/null 2>&1 &
guest /usr/local/bin/rathole-x run --config /etc/rathole-x/armhf-server.toml >/dev/null 2>&1 &
guest /usr/local/bin/rathole-x run --config /etc/rathole-x/armhf-client.toml >/dev/null 2>&1 &

echo "=== runtime status endpoint reports the control channel ==="
command -v jq >/dev/null || fail "jq is required on the host for JSON assertions"
STATE=""
for _ in $(seq 1 20); do
    STATE=$(guest /usr/local/bin/rathole-x status --config /etc/rathole-x/armhf-server.toml --json \
        | jq -r '.result.runtime.services.demo.state // empty' 2>/dev/null || true)
    [ "$STATE" = "connected" ] && break
    sleep 1
done
[ "$STATE" = "connected" ] || fail "server runtime status never reached connected (last: ${STATE:-none})"
guest /usr/local/bin/rathole-x status --config /etc/rathole-x/armhf-server.toml --json \
    | jq -e '.result.runtime.services.demo.control_channel_source' >/dev/null \
    || fail "server runtime status misses control_channel_source"
echo "runtime status: connected, source recorded"

echo "=== TCP data round trip server:$EXPOSED_PORT -> client -> local:$LOCAL_PORT ==="
# The service bind can lag the control-channel "connected" state by a beat
# under emulation, so retry instead of assuming readiness.
REPLY=""
for _ in $(seq 1 15); do
    REPLY=$(echo "$PAYLOAD" | guest /bin/busybox nc -w 5 127.0.0.1 "$EXPOSED_PORT" || true)
    [ "$REPLY" = "$PAYLOAD" ] && break
    sleep 1
done
[ "$REPLY" = "$PAYLOAD" ] || fail "round trip mismatch: want '$PAYLOAD', got '$REPLY'"
echo "round trip: OK"

echo "PASS: armv7 emulated-rootfs acceptance (status endpoint + TCP forwarding)"
