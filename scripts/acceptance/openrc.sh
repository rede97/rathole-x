#!/bin/sh
# rathole-x OpenRC acceptance test. Runs INSIDE an Alpine container as root.
#
# Host usage (from the repo root, after building a static musl binary):
#   cargo zigbuild --release --target x86_64-unknown-linux-musl \
#     --no-default-features --features server,client,rustls,noise,websocket-rustls,hot-reload
#   docker run --rm \
#     -v "$PWD/target/x86_64-unknown-linux-musl/release:/opt/bin:ro" \
#     -v "$PWD/scripts/acceptance/openrc.sh:/opt/acceptance.sh:ro" \
#     alpine sh /opt/acceptance.sh
#
# Every check is fatal: a regression fails the script instead of printing
# and continuing.
set -eu

NAME=accept
SVC=rathole-x-server-$NAME
CONF=/etc/rathole-x/$NAME.toml
BIN=/usr/local/lib/rathole-x/rathole-x

fail() { echo "FAIL: $*" >&2; exit 1; }
assert_file_mode() {
    [ "$(stat -c '%U:%G %a' "$1")" = "$2" ] \
        || fail "$1 ownership/mode: want '$2', got '$(stat -c '%U:%G %a' "$1")'"
}
# BusyBox has no pgrep. The relay child's comm is the binary name;
# supervise-daemon's cmdline contains the same path, so match comm.
relay_pid() {
    for d in /proc/[0-9]*; do
        if [ "$(cat "$d/comm" 2>/dev/null)" = "rathole-x" ]; then
            basename "$d"
            return 0
        fi
    done
    return 1
}

echo "=== setup ==="
apk add --quiet openrc
cp /opt/bin/rathole-x /usr/local/bin/rathole-x
chmod 0755 /usr/local/bin/rathole-x

echo "=== non-root manager commands are rejected ==="
if su -s /bin/sh nobody -c "/usr/local/bin/rathole-x service install server --name $NAME --yes" 2>&1 | grep -qi sudo; then
    echo "non-root install rejected: OK"
else
    fail "non-root service install was not rejected with a sudo hint"
fi

echo "=== install server service ==="
/usr/local/bin/rathole-x service install server --name "$NAME" --yes

echo "=== least-privilege filesystem layout ==="
getent group rathole-x >/dev/null || fail "rathole-x group missing"
getent passwd rathole-x >/dev/null || fail "rathole-x account missing"
[ -x "$BIN" ] || fail "deployed binary $BIN missing"
assert_file_mode "$BIN" "root:root 755"
assert_file_mode /etc/rathole-x "root:rathole-x 750"
assert_file_mode "$CONF" "root:rathole-x 640"
grep -q 'command_user="rathole-x"' /etc/init.d/"$SVC" \
    || fail "init.d script does not run the relay as rathole-x"
grep -q 'command_args_foreground="run --config' /etc/init.d/"$SVC" \
    || fail "init.d script misses command_args_foreground"
grep -q 'capabilities="!cap_chown,.*\^cap_net_bind_service"' /etc/init.d/"$SVC" \
    || fail "init.d script misses the restricted capability grant"

echo "=== service runs and the relay process is NOT root ==="
rc-service "$SVC" status | grep -q started || fail "service not started after install"
sleep 1
RELAY_PID=$(relay_pid) || fail "relay process not found"
[ "$(stat -c '%U' /proc/"$RELAY_PID")" = "rathole-x" ] \
    || fail "relay process owner: want rathole-x, got $(stat -c '%U' /proc/"$RELAY_PID")"
echo "relay pid $RELAY_PID runs as rathole-x: OK"

# 0x400 == CAP_NET_BIND_SERVICE alone; the bounding set in particular must
# not stay full (libcap's IAB parser has no "all" aggregate — every other
# capability is dropped with an explicit !cap_* entry).
CAPBND=$(awk '/^CapBnd:/ { print $2 }' /proc/"$RELAY_PID"/status)
[ "$CAPBND" = "0000000000000400" ] \
    || fail "relay bounding set: want 0000000000000400, got $CAPBND"
echo "relay bounding set restricted to CAP_NET_BIND_SERVICE: OK"

echo "=== runlevel registration (boot autostart) ==="
rc-update show default | grep -q "$SVC" || fail "$SVC missing from default runlevel"

echo "=== hot reload: config add is picked up without a restart ==="
/usr/local/bin/rathole-x config add --name "$NAME" --server "name:echo;bind:127.0.0.1:5202"
sleep 3
rc-service "$SVC" status | grep -q started || fail "service died during hot reload"
[ "$(relay_pid)" = "$RELAY_PID" ] \
    || fail "hot reload restarted the relay process (pid changed)"

echo "=== stop / start control ==="
/usr/local/bin/rathole-x service stop --name "$NAME"
rc-service "$SVC" status | grep -q started && fail "service still started after stop"
/usr/local/bin/rathole-x service start --name "$NAME"
rc-service "$SVC" status | grep -q started || fail "service not started after start"

echo "=== crash respawn (supervise-daemon) ==="
OLD_PID=$(relay_pid) || fail "relay process not found before kill"
kill -9 "$OLD_PID"
sleep 6
NEW_PID=$(relay_pid) || fail "relay was not respawned"
[ "$OLD_PID" != "$NEW_PID" ] || fail "respawn returned the same pid"
rc-service "$SVC" status | grep -q started || fail "service not started after respawn"
echo "respawn $OLD_PID -> $NEW_PID: OK"

echo "=== upgrade replaces the deployed binary and keeps services up ==="
/usr/local/bin/rathole-x upgrade --yes
assert_file_mode "$BIN" "root:root 755"
relay_pid >/dev/null || fail "relay not running after upgrade"

echo "=== uninstall --purge leaves nothing behind ==="
/usr/local/bin/rathole-x service uninstall --purge --yes --name "$NAME"
[ ! -e /etc/init.d/"$SVC" ] || fail "init.d script left behind"
[ ! -e "$CONF" ] || fail "config left behind"
! rc-update show default | grep -q "$SVC" || fail "runlevel entry left behind"
! relay_pid >/dev/null || fail "relay process still running"

echo "=== ALL OPENRC CHECKS PASSED ==="
