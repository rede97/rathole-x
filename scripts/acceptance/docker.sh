#!/bin/bash
# rathole-x Docker runtime acceptance test. Runs on the HOST against the
# locally built image. Proves the container runs the foreground relay as
# non-root without any init system, and that an empty config volume can be
# bootstrapped with the documented commands.
#
# Usage (from the repo root):
#   scripts/acceptance/docker.sh
set -euo pipefail

IMAGE=rathole-x:acceptance
CONTAINER=rathole-x-acceptance
VOLUME=rathole-x-acceptance-conf

fail() { echo "FAIL: $*" >&2; docker rm -f "$CONTAINER" >/dev/null 2>&1 || true; docker volume rm "$VOLUME" >/dev/null 2>&1 || true; exit 1; }
cleanup() { docker rm -f "$CONTAINER" >/dev/null 2>&1 || true; docker volume rm "$VOLUME" >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo "=== build image ==="
docker build -q -t "$IMAGE" . >/dev/null

echo "=== start with an EMPTY named volume ==="
docker volume create "$VOLUME" >/dev/null
docker run -d --name "$CONTAINER" -v "$VOLUME:/etc/rathole-x" "$IMAGE" >/dev/null
sleep 3

echo "=== PID 1 is the foreground relay as non-root (no init system) ==="
docker inspect -f '{{.State.Running}}' "$CONTAINER" | grep -q true \
    || fail "container exited early: $(docker logs "$CONTAINER" 2>&1 | tail -5)"
PID1=$(docker exec "$CONTAINER" sh -c 'cat /proc/1/comm')
[ "$PID1" = "rathole-x" ] || fail "PID 1 is '$PID1', not the foreground rathole-x"
UID1=$(docker exec "$CONTAINER" sh -c 'stat -c %U /proc/1')
[ "$UID1" = "rathole-x" ] || fail "PID 1 user is '$UID1', not rathole-x"
! docker exec "$CONTAINER" sh -c 'command -v systemctl rc-service' >/dev/null 2>&1 \
    || fail "an init system tool is present/running in the container"
echo "PID 1 = rathole-x (user rathole-x), no systemctl/rc-service: OK"

echo "=== hibernates until a config exists ==="
docker logs "$CONTAINER" 2>&1 | grep -qi -e degraded -e 'waiting for a config' \
    || fail "missing the degraded/waiting log line: $(docker logs "$CONTAINER" 2>&1 | tail -3)"

echo "=== bootstrap via the documented commands (no handwritten TOML) ==="
docker exec "$CONTAINER" rathole-x config set -c /etc/rathole-x/rathole-x.toml \
    --client --remote-addr 127.0.0.1:2333 >/dev/null
docker exec "$CONTAINER" rathole-x config add -c /etc/rathole-x/rathole-x.toml \
    myssh --local-addr 127.0.0.1:22 >/dev/null
docker exec "$CONTAINER" test -f /etc/rathole-x/rathole-x.toml \
    || fail "config file was not created"

echo "=== watcher hot-loads the new config ==="
sleep 4
docker logs "$CONTAINER" 2>&1 | grep -qi -e 'config' -e 'service' \
    || fail "no activity after bootstrap: $(docker logs "$CONTAINER" 2>&1 | tail -3)"
docker inspect -f '{{.State.Running}}' "$CONTAINER" | grep -q true \
    || fail "container died after bootstrap: $(docker logs "$CONTAINER" 2>&1 | tail -5)"

echo "=== clean shutdown on docker stop (SIGTERM) ==="
docker stop -t 10 "$CONTAINER" >/dev/null
EXIT_CODE=$(docker inspect -f '{{.State.ExitCode}}' "$CONTAINER")
[ "$EXIT_CODE" = "0" ] || fail "exit code $EXIT_CODE after docker stop (want 0)"

echo "=== ALL DOCKER CHECKS PASSED ==="
