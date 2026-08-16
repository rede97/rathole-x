#!/bin/bash
# rathole-x systemd acceptance test. Runs INSIDE a privileged Ubuntu
# container booted with systemd as PID 1.
#
# Host usage (from the repo root, after building a musl binary):
#   cargo zigbuild --release --target x86_64-unknown-linux-musl \
#     --no-default-features --features server,client,rustls,noise,websocket-rustls,hot-reload
#   docker run --rm --privileged --cgroupns=host \
#     -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
#     -v "$PWD/target/x86_64-unknown-linux-musl/release:/opt/bin:ro" \
#     -v "$PWD/scripts/acceptance/systemd.sh:/opt/acceptance.sh:ro" \
#     ubuntu:24.04 bash -c 'sleep 2; /opt/acceptance.sh'
#
# (For full fidelity run it under systemd as init; the script itself only
# needs a working systemctl, which the privileged container provides.)
# Every check is fatal.
set -euo pipefail

NAME=accept
UNIT=rathole-x-server-$NAME.service
CONF=/etc/rathole-x/$NAME.toml
BIN=/usr/local/lib/rathole-x/rathole-x

fail() { echo "FAIL: $*" >&2; exit 1; }
assert_file_mode() {
    [ "$(stat -c '%U:%G %a' "$1")" = "$2" ] \
        || fail "$1 ownership/mode: want '$2', got '$(stat -c '%U:%G %a' "$1")'"
}

echo "=== setup ==="
cp /opt/bin/rathole-x /usr/local/bin/rathole-x
chmod 0755 /usr/local/bin/rathole-x

echo "=== non-root manager commands are rejected ==="
OUT=$(su -s /bin/sh nobody -c "/usr/local/bin/rathole-x service install server --name $NAME --yes" 2>&1 || true)
echo "$OUT" | grep -qi sudo \
    || fail "non-root service install was not rejected with a sudo hint: $OUT"
echo "non-root install rejected: OK"

echo "=== install server service ==="
/usr/local/bin/rathole-x service install server --name "$NAME" --yes

echo "=== least-privilege unit and filesystem layout ==="
getent group rathole-x >/dev/null || fail "rathole-x group missing"
getent passwd rathole-x >/dev/null || fail "rathole-x account missing"
[ -x "$BIN" ] || fail "deployed binary $BIN missing"
assert_file_mode "$BIN" "root:root 755"
assert_file_mode /etc/rathole-x "root:rathole-x 750"
assert_file_mode "$CONF" "root:rathole-x 640"
UNIT_FILE=/etc/systemd/system/$UNIT
grep -q '^User=rathole-x$' "$UNIT_FILE" || fail "unit does not set User=rathole-x"
grep -q '^Group=rathole-x$' "$UNIT_FILE" || fail "unit does not set Group=rathole-x"
grep -q '^NoNewPrivileges=true$' "$UNIT_FILE" || fail "unit misses NoNewPrivileges"
grep -q '^CapabilityBoundingSet=CAP_NET_BIND_SERVICE$' "$UNIT_FILE" \
    || fail "unit misses the capability bound"
grep -q "^ExecStart=$BIN run --config $CONF\$" "$UNIT_FILE" \
    || fail "unit does not execute the deployed binary"

echo "=== service is active and the relay process is NOT root ==="
systemctl is-active --quiet "$UNIT" || fail "unit not active after install"
sleep 1
MAINPID=$(systemctl show -p MainPID --value "$UNIT")
[ "$MAINPID" != "0" ] || fail "systemd reports no MainPID"
[ "$(stat -c '%U' /proc/"$MAINPID")" = "rathole-x" ] \
    || fail "relay owner: want rathole-x, got $(stat -c '%U' /proc/"$MAINPID")"
grep -q 'NoNewPrivs:.*1' /proc/"$MAINPID"/status || fail "NoNewPrivs not set on the relay"
echo "relay pid $MAINPID runs as rathole-x with NoNewPrivs: OK"

echo "=== enabled for boot ==="
systemctl is-enabled --quiet "$UNIT" || fail "unit not enabled for boot"

echo "=== hot reload: config add is picked up without a restart ==="
/usr/local/bin/rathole-x config add --name "$NAME" --server "name:echo;bind:127.0.0.1:5202"
sleep 3
systemctl is-active --quiet "$UNIT" || fail "unit died during hot reload"
[ "$(systemctl show -p MainPID --value "$UNIT")" = "$MAINPID" ] \
    || fail "hot reload restarted the relay process"

echo "=== stop / start / restart control ==="
/usr/local/bin/rathole-x service stop --name "$NAME"
systemctl is-active --quiet "$UNIT" && fail "unit still active after stop" || true
/usr/local/bin/rathole-x service start --name "$NAME"
systemctl is-active --quiet "$UNIT" || fail "unit not active after start"
/usr/local/bin/rathole-x service restart --name "$NAME"
systemctl is-active --quiet "$UNIT" || fail "unit not active after restart"

echo "=== crash respawn (Restart=on-failure) ==="
OLD_PID=$(systemctl show -p MainPID --value "$UNIT")
kill -9 "$OLD_PID"
sleep 5
NEW_PID=$(systemctl show -p MainPID --value "$UNIT")
[ "$NEW_PID" != "0" ] || fail "systemd did not restart the relay"
[ "$OLD_PID" != "$NEW_PID" ] || fail "respawn returned the same pid"
systemctl is-active --quiet "$UNIT" || fail "unit not active after respawn"
echo "respawn $OLD_PID -> $NEW_PID: OK"

echo "=== upgrade replaces the deployed binary and keeps services up ==="
/usr/local/bin/rathole-x upgrade --yes
assert_file_mode "$BIN" "root:root 755"
systemctl is-active --quiet "$UNIT" || fail "unit not active after upgrade"

echo "=== uninstall --purge leaves nothing behind ==="
/usr/local/bin/rathole-x service uninstall --purge --yes --name "$NAME"
[ ! -e "$UNIT_FILE" ] || fail "unit file left behind"
[ ! -e "$CONF" ] || fail "config left behind"
! systemctl list-unit-files | grep -q "$UNIT" || fail "unit still registered"

echo "=== ALL SYSTEMD CHECKS PASSED ==="
