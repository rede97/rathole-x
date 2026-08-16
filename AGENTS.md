# Repository Guidelines

## Project Overview

`rathole-x` is a Windows-first Rust fork of [rathole](https://github.com/rapiz1/rathole): a secure reverse proxy for NAT traversal. A client behind NAT exposes local TCP/UDP services through a public server.

Non-negotiable product rules:

- Keep `src/protocol.rs` byte/wire compatible with upstream. Do not change bincode message shapes, digests, packet lengths, or protocol versioning for management features.
- One process runs one role. A config containing both `[client]` and `[server]` requires explicit `run --client` or `run --server`; normal deployment uses separate configs/processes.
- Manage configs through `service install` and `config add|set|remove`; do not hand-write or partially write config files from code.
- `--json` commands emit one stdout envelope: `{ "ok": true, "result": ... }` or `{ "ok": false, "error": { "message": ... } }`. Unattended destructive commands require `--yes`.
- Windows service support is implemented. Linux systemd support in `docs/plan-linux-service.md` is a plan, not an implementation.

## Architecture & Data Flow

```text
main.rs
  -> Cli parse, JSON parse errors, UAC relay stdio, tracing, Ctrl-C broadcast
  -> lib.rs::run
       -> command dispatch: run | config | status | service | upgrade | genkey
       -> run_with_config
            -> ConfigWatcherHandle (notify diff)
            -> one single-role run_instance generation
                 -> run_client OR run_server
                 -> RuntimeRegistry -> local Windows status pipe
```

- `src/lib.rs` owns dispatch, JSON envelopes, confirmation policy, run supervision, and `determine_run_mode`.
- `src/config_watcher.rs` emits `ConfigChange::General` for section/non-service changes (restart only the instance generation) and per-service Add/Delete events for live application. Invalid rescans retain the prior config.
- Client control channels authenticate per service; server control channels are keyed by service digest. Data channels are created on demand. The server uses TCP pool size 8 and UDP pool size 1.
- `src/runtime_status.rs` is a local management plane, not proxy protocol. It tracks client/server control channels and listener state. Windows exposes snapshots through an ACL-protected named pipe derived from the canonical config path; `status` merges that snapshot with SCM and static config data. Read-only status must work without UAC; installed directories/configs are user-readable but only admin-writable unless `--allow-user-config`, and status must distinguish `readable`/`missing`/`denied` instead of collapsing access errors to missing.
- Use the `src/platform.rs` façade. `src/platform/windows.rs` owns SCM, UAC, ACL, binary upgrade, and named-pipe code; `src/platform/other.rs` provides planned-platform stubs. Keep `lib.rs` and `main.rs` platform-cfg-free.

## Key Directories

| Path | Purpose |
|---|---|
| `src/` | Library crate and `rathole-x` binary entrypoint. |
| `src/platform/` | Windows SCM/UAC/ACL/runtime-status implementation and non-Windows stubs. |
| `src/transport/` | `Transport` trait plus TCP, TLS, Noise, and WebSocket transports. |
| `tests/` | Integration, authentication, hot-reload tests and TOML fixtures. |
| `examples/` | Upstream-style sample configs and TLS demo material; do not treat sample secrets/keys as production-safe. |
| `docs/` | Upstream documentation plus fork-specific plans/audits. Label planned or inherited material accurately. |
| `scripts/` | Release helper scripts. |
| `.github/workflows/` | Rust CI and Windows release workflow. |

## Development Commands

Use stable Rust and Cargo:

```bash
cargo build
cargo run -- status
cargo test --verbose
cargo test --lib
cargo test --test integration_test
cargo test --test auth_test
cargo test --test hot_reload_test
cargo clippy -- -D warnings
cargo test --verbose --no-default-features --features server,client,rustls,noise,websocket-rustls,hot-reload
cargo check --target x86_64-unknown-linux-gnu --no-default-features --features embedded
```

CI also checks the feature powerset:

```bash
cargo hack check --feature-powerset --no-dev-deps \
  --mutually-exclusive-features default,native-tls,websocket-native-tls,rustls,websocket-rustls
```

On Windows, stop an installed service before commands that relink `target/debug/rathole-x.exe`; a running service can lock that executable. Do not stop a user service without permission.

## Code Conventions & Common Patterns

- Return `anyhow::Result`; add path/action context with `.with_context(...)`; use lowercase actionable `bail!` messages.
- Use Tokio. Process shutdown uses `broadcast::channel<bool>`; instance config updates use `mpsc::channel<ConfigChange>`; cancellation-sensitive loops use `tokio::select!`.
- Shared runtime maps use `Arc<RwLock<_>>`: Tokio locks for async service/control maps, short standard-lock sections for runtime-status snapshots. Never hold a lock across `.await`.
- A server control-channel teardown may clear only the snapshot it owns; replacement channels must not be marked disconnected by stale tasks.
- Every feature-gated branch needs both the real `#[cfg(feature = "x")]` implementation and a diverging `helper::feature_not_compile("x")` branch. `native-tls` and `rustls` are mutually exclusive.
- Preserve config comments/formatting with `toml_edit::DocumentMut`. All writes go through `config_edit::write_atomic`; Windows replacement must preserve the target ACL/owner via `ReplaceFileW`.
- Config schema structs use `deny_unknown_fields`; additive schema fields need defaults for upstream compatibility. Use `MaskedString` for secrets.
- Installed configs have a sibling `version.toml` major-version stamp. `config add|set|remove` must call `check_version_compat`; breaking config schema changes require a major-version bump.
- New JSON commands must be registered in `lib.rs::command_requests_json`, return values rather than printing JSON directly, and use `confirm_action` for destructive behavior.
- Runtime-status data is versioned local IPC. Keep it bounded and non-secret; do not extend `protocol.rs` for status or management operations.

## Important Files

| Path | Why it matters |
|---|---|
| `src/main.rs` | Binary entry, Clap parse-error JSON handling, UAC relay and tracing setup. |
| `src/lib.rs` | CLI dispatch, JSON/confirmation contract, watcher supervision, single-role selection. |
| `src/cli.rs` | Clap 3 command surface, hidden UAC flags, version-major parsing. |
| `src/config.rs` | TOML schema, defaults and runtime validation. |
| `src/config_edit.rs` | Comment-preserving edits, atomic writes, version policy, config skeleton and role gate. |
| `src/config_watcher.rs` | Hot-reload diff and invalid-rescan behavior. |
| `src/client.rs`, `src/server.rs` | Control/data channel lifetimes, hot reload, runtime-state transitions. |
| `src/runtime_status.rs`, `src/status.rs` | Versioned runtime snapshots and human/JSON status rendering. |
| `src/platform/windows.rs` | SCM lifecycle, UAC replay, ACL lockdown, binary upgrade, named-pipe endpoint. |
| `src/protocol.rs` | Frozen upstream-compatible wire format. |
| `src/transport/mod.rs` | Transport contract, socket policy, TLS-backend exclusivity. |
| `Cargo.toml`, `build.rs` | Feature graph, Windows dependencies, profiles, vergen build version. |

## Runtime/Tooling Preferences

- Stable Rust; no `rust-toolchain` pin. The Windows development target is MSVC.
- Use Cargo only. Keep `Cargo.lock` committed. Do not add native libgit2 or a second simultaneous TLS backend.
- `build.rs` intentionally uses vergen without its git feature because native libgit2 is problematic on MSVC.
- `cargo fmt` follows `.rustfmt.toml`; stable may warn that `imports_granularity = "module"` is nightly-only.
- `service upgrade` requires UAC for protected installed binaries: stop services, replace the shared binary, reapply ACLs, then restart services.
- Local Windows release: `./scripts/release.sh <TAG>` requires Bash, PowerShell, and `gh`; it builds `x86_64-pc-windows-msvc` and creates a draft release for `rede97/rathole-x`.

## Testing & QA

- In-source unit tests cover CLI parsing, config editing/version policy, watcher diffs, runtime status, listener state, and Windows file/ACL helpers.
- `tests/integration_test.rs` exercises TCP/UDP forwarding across configured transports. `tests/auth_test.rs` proves wrong tokens are rejected. `tests/hot_reload_test.rs` verifies separate client/server processes survive a General hot-reload restart.
- Integration ports must be free: TCP/UDP tests use control `2333`, exposed `2334-2335`, local `8080-8081`; auth uses `12333`, `12080-12081`; hot reload uses `12340-12344`.
- Prefer behavioral tests for observable contracts: JSON envelope/error paths, config mutation and hot reload, authenticated connection state, listener transitions, and real forwarding. Avoid source-text or plumbing-only tests.
- CI runs `cargo clippy -- -D warnings`, default-feature tests, and a rustls feature-set test on Linux, Windows, and macOS. Keep warnings and feature combinations clean.
- Treat `docs/audit-2026-08-15.md` as a dated audit snapshot, not a live task list. Most upstream-inherited docs/examples use old `rathole` names or placeholder secrets; verify against source before relying on them.
