#!/bin/sh
set -eu

# A named volume is mounted after image construction and initially belongs to
# root. Do the one ownership initialization as root, then replace this process
# with the foreground daemon under the dedicated account. No systemd/OpenRC is
# started inside the container; Docker owns lifecycle and restart behavior.
if [ "$(id -u)" = "0" ]; then
    chown -R rathole-x:rathole-x /etc/rathole-x
    exec su-exec rathole-x:rathole-x "$@"
fi

exec "$@"
