# syntax=docker/dockerfile:1
#
# Multi-stage Alpine image for rathole-x.
#
# In a container, Docker itself is the service manager (restart policy =
# crash respawn, the Docker service = boot autostart, `docker stop` delivers
# SIGTERM for a clean shutdown). It never invokes systemd or OpenRC: the
# entrypoint performs only volume ownership initialization, then execs the
# foreground `rathole-x run` process as the non-root `rathole-x` user.
#
# Build:
#   docker build -t rathole-x .
# Run (config dir is a volume; the container hibernates until a config appears):
#   docker run -d --name rathole-x -v rathole-x-conf:/etc/rathole-x \
#     --restart unless-stopped rathole-x
# Bootstrap the empty volume (no handwritten TOML, no system init):
#   docker exec rathole-x rathole-x config set -c /etc/rathole-x/rathole-x.toml \
#     --client --remote-addr example.com:2333
#   docker exec rathole-x rathole-x config add -c /etc/rathole-x/rathole-x.toml \
#     myssh --local-addr 172.17.0.1:22
#
# The default feature set is the rustls one: native-tls would need a vendored
# OpenSSL build inside Alpine, rustls links statically with no extra deps.

FROM rust:alpine AS builder
# musl-dev: ring (rustls) compiles C and needs the musl headers/linker bits.
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
ARG FEATURES=server,client,rustls,noise,websocket-rustls,hot-reload
RUN cargo build --locked --release --no-default-features --features ${FEATURES}

FROM alpine:latest
# ca-certificates: rustls loads the system roots to verify TLS servers.
# su-exec drops privileges after the minimal named-volume ownership setup.
RUN apk add --no-cache ca-certificates su-exec \
    && addgroup -S rathole-x \
    && adduser -S -D -H -G rathole-x rathole-x \
    && install -d -o rathole-x -g rathole-x -m 0750 /etc/rathole-x
COPY --from=builder /src/target/release/rathole-x /usr/local/bin/rathole-x
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint

# The OS default config path on Linux is /etc/rathole-x/rathole-x.toml;
# `run` hibernates ("Degraded: waiting for a config update") when it is
# missing, so the container can start before any config exists.
VOLUME /etc/rathole-x
ENTRYPOINT ["/usr/local/bin/docker-entrypoint", "rathole-x"]
CMD ["run"]
