# rathole

![rathole-logo](./docs/img/rathole-logo.png)

[![GitHub stars](https://img.shields.io/github/stars/rapiz1/rathole)](https://github.com/rapiz1/rathole/stargazers)
[![GitHub release (latest SemVer)](https://img.shields.io/github/v/release/rapiz1/rathole)](https://github.com/rapiz1/rathole/releases)
![GitHub Workflow Status (branch)](https://img.shields.io/github/actions/workflow/status/rapiz1/rathole/rust.yml?branch=main)
[![GitHub all releases](https://img.shields.io/github/downloads/rapiz1/rathole/total)](https://github.com/rapiz1/rathole/releases)
[![Docker Pulls](https://img.shields.io/docker/pulls/rapiz1/rathole)](https://hub.docker.com/r/rapiz1/rathole)
[![Join the chat at https://gitter.im/rapiz1/rathole](https://badges.gitter.im/rapiz1/rathole.svg)](https://gitter.im/rapiz1/rathole?utm_source=badge&utm_medium=badge&utm_campaign=pr-badge&utm_content=badge)

[English](README.md) | [简体中文](README-zh.md)

A secure, stable and high-performance reverse proxy for NAT traversal, written in Rust

rathole, like [frp](https://github.com/fatedier/frp) and [ngrok](https://github.com/inconshreveable/ngrok), can help to expose the service on the device behind the NAT to the Internet, via a server with a public IP.

# rathole-x (fork)

`rathole-x` is an enhanced fork of [rathole](https://github.com/rapiz1/rathole) that keeps the **wire protocol 100% upstream-compatible** — upstream rathole clients and servers interoperate with `rathole-x` — while adding a subcommand-driven CLI, zero-config-file-handcrafting workflows, and deeper platform integration. The binary name is `rathole-x`.

## Who it is for

`rathole-x` is built for self-hosted, single-admin environments where installing and operating the relay should be safe and uneventful:

- **Personal x64 hosts and home servers** — expose SSH, a NAS, or a home-lab service behind NAT; installed once as a boot-enabled systemd/OpenRC service.
- **Public servers (VPS)** — the relay side, deployed with the same one-command service install.
- **Raspberry Pi and other ARM boards** — Debian/Raspberry Pi OS (32-bit armv7 and 64-bit) and Alpine run the static musl binaries (zero system dependencies); a Docker image is available for container-first setups.
- **Personal Windows machines** — a real SCM service installed through a UAC-guided flow, not a scheduled-task workaround.

In every scenario the same secure-by-default tooling does the work: `service install` registers a least-privilege system service, `config add|set|remove` manages configuration without hand-editing TOML, `status` inspects runtime state without elevation, and hot reload applies changes in place.

## Design philosophy

- **One binary, subcommand-driven CLI.** Human-friendly: interactive prompts plus auto-generated tokens and noise keys. Agent-friendly: everything is available as flags, with `--json` output and a `--yes` non-interactive mode.
- **Zero config-file handcrafting.** `service install <server|client>` deploys a system service with a role-specific skeleton config; `config add`/`config set` manage everything; hot reload applies changes without restarting.
- **One process, one role.** Every foreground or installed process runs exactly one role (server or client) from one role-specific config. To run both on a host, configure and run two independent processes.
- **Upstream-compatible wire protocol.** `rathole-x` speaks the same wire protocol as upstream rathole, so the two interoperate.
- **Platform integration.** Windows SCM service plus UAC elevation; Linux systemd and OpenRC services (see [docs/linux-service.md](docs/linux-service.md)).
- **Secure defaults.** Tokens are mandatory; config edits are gated by the actual file permissions — the CLI probes whether the current user can write the config and elevates (UAC) only when not; the service binary is copied into `ProgramData` and is not replaceable by non-admins. Read-only status (SCM state, runtime connection snapshot, and the installed config tree) is readable by local users without UAC; `permission denied`, `missing`, and `unavailable` are reported distinctly.

## Agent skills

`rathole-x` ships agent-consumable skills for AI coding and automation assistants that operate the CLI on a user's behalf. The CLI is agent-friendly by design — every operation is a subcommand, `--json` emits exactly one machine-readable envelope, `--yes` enables unattended use — and the skill documents the full workflow: downloading and verifying the release binary for the host architecture, installing the service, managing entries with `config add|set|remove`, and verifying with `status --json`.

- [rathole-x-config](skills/rathole-x-config/SKILL.md) — configure and deploy relays end-to-end through the CLI: service install, config management, transport selection, token handling, and failure triage. The `.md` front matter includes a `description` field usable as a tool/agent definition.

## New features over upstream

- **Subcommand-driven CLI** — `run` (run the daemon), `config add|remove|list|set` (manage the service configuration), `status` (service state + config tree), `genkey` (generate a noise keypair), `service install|uninstall|start|stop|restart` (system service lifecycle), `upgrade` (update the installed binary). See [CLI reference](#cli-reference) for flags.
- **Single-role execution** — a config containing both sections must be run explicitly with `run --server` or `run --client`; normal deployment uses two independently configured processes.
- **Auto-generated tokens and noise keys** — `config add`/`config set` generate them when omitted.
- **Hot reload via atomic writes** — `config add`/`config set`/`config remove` rewrite the config atomically; the running service hot-reloads it without a restart.
- **Windows service install** — `service install server|client --name <n>` registers a named AutoStart SCM service that runs as `NT AUTHORITY\LocalService` with a **restricted per-service SID**, never LocalSystem. The shared protected binary is read/execute-only to installed `NT SERVICE\rathole-x-<role>-<n>` SIDs; each service gets a protected log directory; installed configs remain read-only for local users so `status` works without UAC, while only Administrators can edit by default (`--allow-user-config` grants Users write).
- **Linux systemd / OpenRC service install** — the same `service install server|client --name <n>` works on both init systems (detected at runtime): systemd gets a boot-enabled restricted non-root unit; OpenRC distros (Alpine, Gentoo) get an openrc-run script that runs the daemon as `rathole-x` via `command_args_foreground`, with root retaining only supervisor/pid/log lifecycle duties. Both allow low ports only through `CAP_NET_BIND_SERVICE`.
- **Role skeleton auto-creation** — `service install` creates a role-specific skeleton config when none exists.

## Quick start (Windows)

1. Install the services from an elevated shell — one named process per role; to run both server and client on a host, install two services. You will be asked to confirm (or pass `--yes` to skip; unattended and JSON invocations fail with an actionable error unless `--yes` is passed). A role skeleton config is created if missing, the binary is copied next to the configs, and a Windows SCM service (AutoStart) is registered with UAC elevation:

```bash
# Server (public IP)
./rathole-x service install server --yes --name relay

# Client (behind NAT)
./rathole-x service install client --yes --name home-nas
```

2. Add a service. The demo below exposes a NAS ssh service: the same name pairs the server and client sides, and tokens are auto-generated:

```bash
# Server (public IP): expose port 5202 to the Internet
./rathole-x config add --name relay --server "name:my_nas_ssh;bind:0.0.0.0:5202"

# Client (behind NAT): point at the server once, then forward to the NAS
# ssh daemon on port 22
./rathole-x config set --name home-nas --client --remote-addr myserver.com:2333
./rathole-x config add --name home-nas --client "name:my_nas_ssh;local:127.0.0.1:22"
```

3. Check the service state and the configuration tree:

```bash
./rathole-x status
```

4. Uninstall a service. `version.toml` is removed with the last service; the config file is kept unless `--purge` is passed:

```bash
./rathole-x service uninstall --yes --name relay
./rathole-x service uninstall --yes --name home-nas
```

> **Linux systemd / OpenRC**: run the same commands under `sudo` (`sudo rathole-x service install server --yes --name relay`). systemd writes, enables and starts `rathole-x-server-relay.service`; OpenRC (Alpine, Gentoo) writes `/etc/init.d/rathole-x-server-relay` and registers it through `rc-update add default`. Both run the protected root:root `0755` binary at `/usr/local/lib/rathole-x/rathole-x` as the dedicated non-login `rathole-x:rathole-x` account, never root. Linux has no self-elevation: non-root runs fail with `please run with sudo`. See [docs/linux-service.md](docs/linux-service.md).

## Service lifecycle

- `rathole-x service start|stop|restart [--name N | --all]` — drive the SCM state of installed services (UAC elevated when needed).
- `rathole-x upgrade --yes` — update the installed binary in place: stops every service, replaces the shared binary with the running one, starts them again.
- `rathole-x service uninstall --yes --all` — remove every installed service and the shared binary while keeping configs; add `--purge` to remove every config too.
- A normal `service uninstall --yes` leaves the kept config user-deletable, and uninstalling an already-removed service cleans leftover files WITHOUT elevation.

## Config schema versioning

The `version.toml` file (written by `service install`) stamps the **major version** of the rathole-x build that installed the service. `config add`/`config set`/`config remove` refuse to touch a config whose stamped major version differs from the running CLI:

```text
This config is managed by rathole-x v0 but this CLI is v1.
Reinstall the service to upgrade: `rathole-x service uninstall --yes` then `rathole-x service install <server|client> --yes`.
```

Read-only commands (`status`, `config list`, `run`) and `install`/`uninstall` are never blocked by the stamp. Configs without a stamp (user-managed files) have no restriction.

**Rule for developers: any breaking config schema change (renamed/removed/retagged fields, changed semantics or defaults) MUST bump the major version.** Purely additive changes (new optional fields with serde defaults) do not require one. The stamp lives in `version.toml`, so the rathole config file itself stays parseable by upstream rathole.

## Docker

On Docker hosts, skip the init system entirely: Docker is the lifecycle manager (`restart` = crash respawn, the daemon = boot autostart, `docker stop` sends SIGTERM). The image **does not start or call systemd/OpenRC**. Its entrypoint does only the minimal mounted-config ownership initialization, then `exec`s foreground `rathole-x run` as non-root `rathole-x`; Docker manages the process and the daemon hot-reloads the mounted config.

```bash
docker build -t rathole-x .
docker run -d --name rathole-x -v rathole-x-conf:/etc/rathole-x --restart unless-stopped rathole-x
# The container starts empty and hibernates until a config exists. Bootstrap
# the mounted volume atomically; this needs neither TOML editing nor an init:
docker exec rathole-x rathole-x config set -c /etc/rathole-x/rathole-x.toml --client --remote-addr myserver.com:2333
docker exec rathole-x rathole-x config add -c /etc/rathole-x/rathole-x.toml myssh --local-addr 172.17.0.1:22
```

A ready-made [docker-compose.yml](examples/docker/docker-compose.yml) lives under `examples/docker/`. Client mode usually wants `network_mode: host` (or a `local` addr reachable from the container, like the docker bridge `172.17.0.1`); server mode can publish ports instead.

## Runtime connection status

On Windows, `status` also queries an ACL-protected **local-only, read-only** named pipe derived from the canonical config path; on Linux it connects to an abstract-namespace Unix socket under the same derived name. On both platforms any local user may request one bounded snapshot, no network listener is added, and Windows remote clients are rejected by the pipe. The `runtime` field is a local snapshot, not a network probe: it includes schema version, role, process ID and capture time; client services report `connecting`, `connected`, `retrying`, or `stopped` plus configured/resolved **control target** and connection/error times; server services report `waiting`, `connected`, or `stopped` plus the authenticated control-channel source and connection/disconnect metadata. It never exposes tokens, keys, payloads, or traffic.

`runtime: null` is normal when the process is stopped, uses an old binary or another config, or has not created its endpoint yet; `runtime_availability.reason` explains it without failing the static status result.

## CLI reference

`config add|remove|list|set`, `status`, service lifecycle actions, and `upgrade` accept `--json`. JSON stdout is exactly one envelope: `{ "ok": true, "result": ... }` or `{ "ok": false, "error": { "message": ... } }`; progress and diagnostics stay off stdout. `config remove`, service install/uninstall, and upgrade require confirmation: use `--yes` for JSON or unattended invocation, otherwise they fail with an actionable error. The upstream positional `./rathole config.toml` form is **not supported** in this fork — run the daemon with `run -c CONFIG` (with no `-c`, the OS default path is used: Windows `%ProgramData%\rathole-x\rathole-x.toml`, Linux `/etc/rathole-x.toml`).

- `run [-c CONFIG] [--server|--client]` — run exactly one daemon role. A dual-section config requires explicit `--server` or `--client`.
- `config add [<NAME>] [--client SPEC]... [--server SPEC]... [--bind-addr A] [--local-addr A] [--token T] [--noise] [--noise-key K] [--type tcp|udp] [-c] [--name N] [--json] [--yes]` — first-class repeatable forwarding-service creation. Client specs use `name`, `local`, `token`, and `type`; set the one config-global remote first with `config set --client --remote-addr A`. Batch specs make one atomic write; interactive add accepts corrected field input.
- `config remove <NAME> [-c | --name N] [--json] [--yes]` — remove a service after confirmation.
- `config list [-c] [--name N] [--json]` — list services.
- `config set <--client|--server> [global fields] [-c] [--name N] [--json]` — set global fields: `--remote-addr`, `--bind-addr`, `--default-token`, `--prefer-ipv6`, `--heartbeat-timeout`, `--retry-interval`, `--heartbeat-interval`, `--transport tcp|tls|noise|websocket`, `--noise`, `--noise-key`, `--trusted-root`, `--hostname`, `--pkcs12`, `--pkcs12-password`, `--ws-tls`, `--nodelay`, `--keepalive-secs`, `--keepalive-interval`, `--proxy`.
- `status [-c] [--name N] [--json]` — print the service state plus the config tree; without `--name` every installed service is listed.
- `genkey [--curve x25519|x448]` — generate a noise keypair.
- `service install <server|client> --yes [-c] [--name N] [--allow-user-config]` — install a system service (exactly one role): creates a role skeleton config if missing, copies the binary next to the config, writes `uninstall-<N>.bat`, and registers a Windows SCM service (AutoStart) with UAC. `--name` defaults to "default". Runs after an interactive confirmation; `--yes` skips it (required in unattended/JSON invocations).
- `service uninstall --yes [--name N] [-c] [--purge] [--all]` — uninstall the named service; `--all` removes every installed service and the shared binary while keeping configs, and `--all --purge` removes every config as well. `version.toml` is removed with the last service.
- `service start|stop|restart [--name N | --all]` — drive the SCM state of installed services (UAC elevated when needed).
- `upgrade --yes` — stop every service, replace the shared binary with the running one, start them again.

---

> The following is the original README (upstream rathole documentation)

---

<!-- TOC -->

- [rathole](#rathole)
  - [Features](#features)
  - [Quickstart](#quickstart)
  - [Configuration](#configuration)
    - [Logging](#logging)
    - [Tuning](#tuning)
  - [Benchmark](#benchmark)
  - [Planning](#planning)
- [rathole-x (fork)](#rathole-x-fork)

<!-- /TOC -->

## Features

- **High Performance** Much higher throughput can be achieved than frp, and more stable when handling a large volume of connections. See [Benchmark](#benchmark)
- **Low Resource Consumption** Consumes much fewer memory than similar tools. See [Benchmark](#benchmark). [The binary can be](docs/build-guide.md) **as small as ~500KiB** to fit the constraints of devices, like embedded devices as routers.
- **Security** Tokens of services are mandatory and service-wise. The server and clients are responsible for their own configs. With the optional Noise Protocol, encryption can be configured at ease. No need to create a self-signed certificate! TLS is also supported.
- **Hot Reload** Services can be added or removed dynamically by hot-reloading the configuration file. HTTP API is WIP.


## CLI quick start (rathole-x)

The `rathole-x` binary is fully subcommand-based; running it bare prints the help. Start the daemon with `run -c` (the upstream positional `./rathole config.toml` form is not supported in this fork).

```bash
# Add forwarding services repeatedly; set the client control-channel server
# once per client config before its first add.
./rathole-x config set --client --remote-addr myserver.com:2333
./rathole-x config add --client "name:my_nas_ssh;local:127.0.0.1:22"
./rathole-x config add --client "name:database;local:127.0.0.1:5432"

# Tune [client]/[server] global fields and transports without editing the file
./rathole-x config set --server --bind-addr 0.0.0.0:2333 --noise            # generates a noise keypair
./rathole-x config set --client --remote-addr myserver.com:2333 --noise-key <SERVER_PUBLIC_KEY>
./rathole-x config set --server --transport tls --pkcs12 identity.pfx --pkcs12-password 1234
./rathole-x config set --client --default-token shared --heartbeat-timeout 60

# Show the service state and configuration as a tree (--json for scripts)
./rathole-x status
./rathole-x status --json

# Inspect and edit the config it maintains
./rathole-x config list -c config.toml
./rathole-x config remove my_nas_ssh -c config.toml

# --json writes exactly one envelope to stdout:
# {"ok":true,"result":...} or {"ok":false,"error":{"message":"..."}}.
# Confirmation-required actions need --yes in JSON/unattended use.
./rathole-x config add --client "name:my_nas_ssh;local:127.0.0.1:22" --json

# Batch-add several services in one write; add remains repeatable for later ports.
./rathole-x config add --client "name:nas;local:127.0.0.1:22" \
  --client "name:db;local:127.0.0.1:5432"

# Generate a noise keypair (replaces the removed --genkey flag)
./rathole-x genkey

# A run process selects exactly one role. A hand-written dual-section config
# requires --server or --client; production services remain separate.
./rathole-x run --server -c server.toml
# Install named services: exactly one role per service, one config file per
# service. A role-specific skeleton config is created automatically.
./rathole-x service install server --yes --name relay
./rathole-x service install client --yes --name home-nas

# Grant normal users write access to the config: the CLI detects the
# permission at runtime and skips UAC. No policy file is stored.
./rathole-x service install server --yes --name relay --allow-user-config

# Uninstall: version.toml is removed with the last service; the config file
# is kept unless --purge is given.
./rathole-x service uninstall --yes --name relay
./rathole-x service uninstall --yes --purge --name home-nas
```

## Quickstart

A full-powered `rathole` can be obtained from the [release](https://github.com/rapiz1/rathole/releases) page. Or [build from source](docs/build-guide.md) **for other platforms and minimizing the binary**. A [Docker image](https://hub.docker.com/r/rapiz1/rathole) is also available.

The usage of `rathole` is very similar to frp. If you have experience with the latter, then the configuration is very easy for you. The only difference is that configuration of a service is split into the client side and the server side, and a token is mandatory.


1. On the server which has a public IP

Create `server.toml` with the following content and accommodate it to your needs.

```toml
# server.toml
[server]
bind_addr = "0.0.0.0:2333" # `2333` specifies the port that rathole listens for clients

[server.services.my_nas_ssh]
token = "use_a_secret_that_only_you_know" # Token that is used to authenticate the client for the service. Change to an arbitrary value.
bind_addr = "0.0.0.0:5202" # `5202` specifies the port that exposes `my_nas_ssh` to the Internet
```

Then run:

```bash
./rathole server.toml
```

2. On the host which is behind the NAT (your NAS)

Create `client.toml` with the following content and accommodate it to your needs.

```toml
# client.toml
[client]
remote_addr = "myserver.com:2333" # The address of the server. The port must be the same with the port in `server.bind_addr`

[client.services.my_nas_ssh]
token = "use_a_secret_that_only_you_know" # Must be the same with the server to pass the validation
local_addr = "127.0.0.1:22" # The address of the service that needs to be forwarded
```

Then run:

```bash
./rathole client.toml
```

3. Now the client will try to connect to the server `myserver.com` on port `2333`, and any traffic to `myserver.com:5202` will be forwarded to the client's port `22`.

So you can `ssh myserver.com:5202` to ssh to your NAS.

To run `rathole` run as a background service on Linux, checkout the [systemd examples](./examples/systemd).

## Configuration

`rathole` can automatically determine to run in the server mode or the client mode, according to the content of the configuration file, if only one of `[server]` and `[client]` block is present, like the example in [Quickstart](#quickstart).

But the `[client]` and `[server]` block can also be put in one file. Then on the server side, run `rathole --server config.toml` and on the client side, run `rathole --client config.toml` to explicitly tell `rathole` the running mode.

Before heading to the full configuration specification, it's recommend to skim [the configuration examples](./examples) to get a feeling of the configuration format.

See [Transport](./docs/transport.md) for more details about encryption and the `transport` block.

Here is the full configuration specification:

```toml
[client]
remote_addr = "example.com:2333" # Necessary. The address of the server
default_token = "default_token_if_not_specify" # Optional. The default token of services, if they don't define their own ones
heartbeat_timeout = 40 # Optional. Set to 0 to disable the application-layer heartbeat test. The value must be greater than `server.heartbeat_interval`. Default: 40 seconds
retry_interval = 1 # Optional. The interval between retry to connect to the server. Default: 1 second

[client.transport] # The whole block is optional. Specify which transport to use
type = "tcp" # Optional. Possible values: ["tcp", "tls", "noise"]. Default: "tcp"

[client.transport.tcp] # Optional. Also affects `noise` and `tls`
proxy = "socks5://user:passwd@127.0.0.1:1080" # Optional. The proxy used to connect to the server. `http` and `socks5` is supported.
nodelay = true # Optional. Determine whether to enable TCP_NODELAY, if applicable, to improve the latency but decrease the bandwidth. Default: true
keepalive_secs = 20 # Optional. Specify `tcp_keepalive_time` in `tcp(7)`, if applicable. Default: 20 seconds
keepalive_interval = 8 # Optional. Specify `tcp_keepalive_intvl` in `tcp(7)`, if applicable. Default: 8 seconds

[client.transport.tls] # Necessary if `type` is "tls"
trusted_root = "ca.pem" # Necessary. The certificate of CA that signed the server's certificate
hostname = "example.com" # Optional. The hostname that the client uses to validate the certificate. If not set, fallback to `client.remote_addr`

[client.transport.noise] # Noise protocol. See `docs/transport.md` for further explanation
pattern = "Noise_NK_25519_ChaChaPoly_BLAKE2s" # Optional. Default value as shown
local_private_key = "key_encoded_in_base64" # Optional
remote_public_key = "key_encoded_in_base64" # Optional

[client.transport.websocket] # Necessary if `type` is "websocket"
tls = true # If `true` then it will use settings in `client.transport.tls`

[client.services.service1] # A service that needs forwarding. The name `service1` can change arbitrarily, as long as identical to the name in the server's configuration
type = "tcp" # Optional. The protocol that needs forwarding. Possible values: ["tcp", "udp"]. Default: "tcp"
token = "whatever" # Necessary if `client.default_token` not set
local_addr = "127.0.0.1:1081" # Necessary. The address of the service that needs to be forwarded
nodelay = true # Optional. Override the `client.transport.nodelay` per service
retry_interval = 1 # Optional. The interval between retry to connect to the server. Default: inherits the global config

[client.services.service2] # Multiple services can be defined
local_addr = "127.0.0.1:1082"

[server]
bind_addr = "0.0.0.0:2333" # Necessary. The address that the server listens for clients. Generally only the port needs to be change.
default_token = "default_token_if_not_specify" # Optional
heartbeat_interval = 30 # Optional. The interval between two application-layer heartbeat. Set to 0 to disable sending heartbeat. Default: 30 seconds

[server.transport] # Same as `[client.transport]`
type = "tcp"

[server.transport.tcp] # Same as the client
nodelay = true
keepalive_secs = 20
keepalive_interval = 8

[server.transport.tls] # Necessary if `type` is "tls"
pkcs12 = "identify.pfx" # Necessary. pkcs12 file of server's certificate and private key
pkcs12_password = "password" # Necessary. Password of the pkcs12 file

[server.transport.noise] # Same as `[client.transport.noise]`
pattern = "Noise_NK_25519_ChaChaPoly_BLAKE2s"
local_private_key = "key_encoded_in_base64"
remote_public_key = "key_encoded_in_base64"

[server.transport.websocket] # Necessary if `type` is "websocket"
tls = true # If `true` then it will use settings in `server.transport.tls`

[server.services.service1] # The service name must be identical to the client side
type = "tcp" # Optional. Same as the client `[client.services.X.type]
token = "whatever" # Necessary if `server.default_token` not set
bind_addr = "0.0.0.0:8081" # Necessary. The address of the service is exposed at. Generally only the port needs to be change.
nodelay = true # Optional. Same as the client

[server.services.service2]
bind_addr = "0.0.0.1:8082"
```

### Logging

`rathole`, like many other Rust programs, use environment variables to control the logging level. `info`, `warn`, `error`, `debug`, `trace` are available.

```shell
RUST_LOG=error ./rathole config.toml
```

will run `rathole` with only error level logging.

If `RUST_LOG` is not present, the default logging level is `info`.

### Tuning

From v0.4.7, rathole enables TCP_NODELAY by default, which should benefit the latency and interactive applications like rdp, Minecraft servers. However, it slightly decreases the bandwidth.

If the bandwidth is more important, TCP_NODELAY can be opted out with `nodelay = false`.

## Benchmark

rathole has similar latency to [frp](https://github.com/fatedier/frp), but can handle a more connections, provide larger bandwidth, with less memory usage.

For more details, see the separate page [Benchmark](./docs/benchmark.md).

**However, don't take it from here that `rathole` can magically make your forwarded service faster several times than before.** The benchmark is done on local loopback, indicating the performance when the task is cpu-bounded. One can gain quite a improvement if the network is not the bottleneck. Unfortunately, that's not true for many users. In that case, the main benefit is lower resource consumption, while the bandwidth and the latency may not improved significantly.

![http_throughput](./docs/img/http_throughput.svg)
![tcp_bitrate](./docs/img/tcp_bitrate.svg)
![udp_bitrate](./docs/img/udp_bitrate.svg)
![mem](./docs/img/mem-graph.png)

## Planning

- [ ] HTTP APIs for configuration

[Out of Scope](./docs/out-of-scope.md) lists features that are not planned to be implemented and why.
