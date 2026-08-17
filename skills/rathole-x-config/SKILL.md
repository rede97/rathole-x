---
name: rathole-x-config
description: "Configure and deploy rathole-x relays for a user: gather requirements, apply changes exclusively through the rathole-x CLI (never hand-edit TOML), verify with status. Covers server/client service install, config add/set/remove, transports (tcp/tls/noise/websocket), and --json agent workflows."
---

# rathole-x Configuration Assistant

Guide an AI agent through configuring `rathole-x` (a NAT-traversal reverse
proxy) on behalf of a user. The CLI is designed for agent use: every mutation
is a subcommand, `--json` emits one machine-readable envelope, and hot reload
applies changes without restarts.

## Non-negotiable rules

1. **Never hand-edit TOML.** All config changes go through
   `rathole-x config add|set|remove`. Hand-written edits bypass validation,
   version stamps, and atomic writes.
2. **Read before write.** Start with `rathole-x status --json` (and
   `config list --json`) to learn the current state; never assume.
3. **Destructive or privileged operations need `--yes`.** Without it,
   unattended invocations fail with an actionable error. Service lifecycle
   commands (`service install|uninstall|start|stop|restart`) need root on
   Linux (`sudo`) / elevation on Windows.
4. **Never stop or uninstall a service the user did not ask you to touch.**
   `status` is read-only and safe; everything under `service` is not.
5. **One process, one role.** A config holds exactly one of `[server]` or
   `[client]`. Both roles on one host = two services with two configs.
6. **Tokens are secrets.** Let the CLI auto-generate them; never invent weak
   ones, never echo them into logs or chat. Outputs mask them as `••••`.

## Download the binary first (no manual browsing)

The agent should download the release asset for the detected host
architecture itself, verify it, and deploy it; never ask the user to find a
download page. Project and release endpoint:

- Repository: `https://github.com/rede97/rathole-x`
- Latest release API: `https://api.github.com/repos/rede97/rathole-x/releases/latest`
- Asset pattern: `rathole-x-<rust-target>.zip` on Windows and
  `rathole-x-<rust-target>.tar.gz` on Linux.

Supported release targets:

| Host | Rust target | Asset |
|---|---|---|
| Windows x64 | `x86_64-pc-windows-msvc` | `rathole-x-x86_64-pc-windows-msvc.zip` |
| Windows x86 | `i686-pc-windows-msvc` | `rathole-x-i686-pc-windows-msvc.zip` |
| Windows ARM64 | `aarch64-pc-windows-msvc` | `rathole-x-aarch64-pc-windows-msvc.zip` |
| Linux x86_64 | `x86_64-unknown-linux-musl` | `rathole-x-x86_64-unknown-linux-musl.tar.gz` |
| Linux x86 | `i686-unknown-linux-musl` | `rathole-x-i686-unknown-linux-musl.tar.gz` |
| Linux ARMv7 | `armv7-unknown-linux-musleabihf` | `rathole-x-armv7-unknown-linux-musleabihf.tar.gz` |
| Linux ARM64 | `aarch64-unknown-linux-musl` | `rathole-x-aarch64-unknown-linux-musl.tar.gz` |

Linux agent workflow (download a pinned release, verify, then deploy):

```bash
TAG=v0.5.5
TARGET=$(case "$(uname -m)" in
  x86_64) echo x86_64-unknown-linux-musl ;;
  i686) echo i686-unknown-linux-musl ;;
  armv7*|armhf) echo armv7-unknown-linux-musleabihf ;;
  aarch64|arm64) echo aarch64-unknown-linux-musl ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac)
curl -fL "https://github.com/rede97/rathole-x/releases/download/$TAG/rathole-x-$TARGET.tar.gz" -o /tmp/rathole-x.tar.gz
tar -xzf /tmp/rathole-x.tar.gz -C /tmp
/tmp/rathole-x --version
```

For the newest published release, resolve the tag through the GitHub API
instead of guessing, then use the matching `browser_download_url` in the
release response. Windows agents should use the matching `.zip`, expand it,
and run `.\rathole-x.exe --version` before `service install`.

If a release has no asset for the detected architecture, stop and report the
missing target; do not substitute another architecture.

## Requirements to gather first

- **Role**: is this host the public relay (`server`) or the NATed machine
  exposing local services (`client`)?
- **Server side**: bind address/port (default convention `0.0.0.0:2333`),
  which services (name, `bind_addr` to expose, type tcp/udp).
- **Client side**: the server's public `remote_addr`, which local services
  (name, `local_addr`, type).
- **Transport**: plain `tcp`, `tls`, `noise`, or `websocket`. `noise` and
  `tls` need key material — use `rathole-x genkey` for noise and let
  `config add` auto-generate keys when possible.
- **Naming**: service names must match `[A-Za-z0-9_-]`.

If any of these are missing, ask the user — do not guess public addresses
or which local service to expose.

## Workflow

### 1. Inspect

```bash
rathole-x status --json                      # service state + config tree + runtime snapshot
rathole-x config list -c <config> --json     # installed config contents (masked)
```

The JSON envelope is exactly one of:
`{"ok": true, "result": ...}` or `{"ok": false, "error": {"message": ...}}`.
On `ok: false`, surface `error.message` to the user verbatim; do not retry
blindly.

### 2. Install (first deployment on a host)

```bash
sudo rathole-x service install server --name <n> --yes   # or: client
```

This deploys the protected binary, creates a least-privilege system service
(systemd/OpenRC on Linux, SCM on Windows) running as the `rathole-x`
account / a restricted service SID, writes a role skeleton config, and starts
it. Installed configs live at `/etc/rathole-x/<n>.toml` (Linux) or under the
install directory (Windows).

### 3. Configure

Server example (expose SSH of the client as :2222 on the server):

```bash
sudo rathole-x config set -c /etc/rathole-x/<n>.toml --server --bind-addr 0.0.0.0:2333 --yes
sudo rathole-x config add -c /etc/rathole-x/<n>.toml myssh --type tcp \
    --bind-addr 0.0.0.0:2222 --yes
```

Client example (forward local :22):

```bash
sudo rathole-x config set -c /etc/rathole-x/<n>.toml --client \
    --remote-addr <server-public-ip>:2333 --yes
sudo rathole-x config add -c /etc/rathole-x/<n>.toml myssh --type tcp \
    --local-addr 127.0.0.1:22 --yes
```

The token is auto-generated on `config add` and shown in that command's
result — add the service on the peer with the same value:
`config add -c <peer-config> <name> --token <value> ...`. To set one shared
token for every service lacking its own, use
`config set -c <config> --default-token <value> --yes`. Transfer tokens only
through a secure channel.

Migrating an existing upstream rathole or older rathole-x config file into a
managed one needs no manual TOML either:

```bash
sudo rathole-x config import /path/to/old.toml -c /etc/rathole-x/<n>.toml --json
```

The import is additive and non-destructive: supported global fields,
transport settings and service entries are copied; entries already present
in the target are skipped (never overwritten); unknown keys are reported
under `ignored`; entries with a missing address or unusable token are
reported under `skipped.invalid_services` with a reason. The old file must
declare exactly one of `[client]`/`[server]`, matching the target's role.

### 4. Verify

```bash
rathole-x status --json
```

- `result.runtime.services.<name>.state` should become `connected`
  (`retrying`/`waiting` means the peer side is missing or the token/address
  is wrong; `result.runtime.services.<name>.last_error.message` says why).
- `runtime: null` means the process is not running — check
  `systemctl status rathole-x-<role>-<n>` (or `rc-service`), never guess.

Config edits hot-reload; no restart is needed after `config add|set|remove`.

## Transport selection

| Need | Use |
|---|---|
| Trusted network / quick setup | `tcp` (default) |
| Encryption without certificates | `noise` (`rathole-x genkey` for a keypair) |
| PKI / organizational certs | `tls` (rustls build on Linux) |
| Traversing HTTP-only proxies/CDN | `websocket` (+ `websocket-rustls` for TLS) |

## Common failure triage

| Symptom in `status --json` | Likely cause |
|---|---|
| `state: retrying`, error `connection refused` | server not running / wrong `remote_addr` / firewall |
| `state: retrying`, error mentioning auth | token mismatch between the two configs |
| server `state: waiting` | no client connected yet — configure the client side |
| `runtime: null` + service state not `active` | service not installed or stopped |
| `config.access: denied` | re-run the mutating command with `sudo` |

## Anti-patterns

- Editing `/etc/rathole-x/*.toml` with an editor or `sed` (use `config set`).
- `systemctl restart` after a config edit (hot reload already applied it).
- Reinstalling a service to "fix" a config problem (fix the config instead).
- Copying tokens over chat/logs; regenerate with `config set` instead.
- Running `rathole-x run` in the foreground for production (install a service).
