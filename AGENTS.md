# Repository Guidelines

## Project Overview

`rathole-x` is an enhanced fork of [rathole](https://github.com/rapiz1/rathole) — a secure, high-performance reverse proxy for NAT traversal. A client behind NAT exposes local services through a server with a public IP.

Fork positioning (must stay true):

- **Wire protocol 100% upstream-compatible** (`src/protocol.rs` is byte-frozen; never change message shapes).
- Subcommand-driven CLI (`rathole-x` binary) with interactive + agent-friendly (`--json`, `--yes`) modes.
- Zero handcrafted config files: `service install` + `config add/set` manage everything; hot reload applies changes without restart.
- One process can run BOTH server and client (dual mode) when the config has both sections — daemon `run` only; installed services are strictly single-role (one service per role, install two to run both).
- Windows: SCM service + UAC elevation + ACL-probed config permission model with a `version.toml` version stamp (no separate policy file). Linux systemd: planned only (`docs/plan-linux-service.md`).
- Upstream boundaries in `docs/out-of-scope.md` still apply (no HTTP domain forwarding, no app-layer logging, etc.).

## Architecture & Data Flow

```
main.rs (parse Cli, stdio redirect for UAC relay, tracing init)
  └─ lib.rs::run → dispatch_command
       ├─ run → run_with_config
       │    ├─ ConfigWatcherHandle (notify): watches config file, diffs, emits ConfigChange
       │    └─ run_instance per ConfigChange::General
       │         ├─ RunMode::Client → run_client
       │         ├─ RunMode::Server → run_server
       │         └─ RunMode::Both   → both halves via tokio::try_join!, one mpsc channel each,
       │                              service events fanned out to every half
       ├─ config {add|remove|list|set} → config_edit (toml_edit, atomic write) → file event → hot reload
       ├─ status → tree renderer + SCM query
       └─ service {install|uninstall|start|stop|restart|run} → platform::install_service/uninstall_service/control_service/run_service (UAC relay + SCM)
```

**Hot reload loop**: CLI writes config (temp file + rename, `write_atomic`) → notify event → watcher rescans → `calculate_events` diffs → `ServerChange/ClientChange::Add/Delete` hot-applied per half, `General` restarts the instance in-process. Invalid rescans keep the old config (`src/config_watcher.rs`).

**Connection flow**: client `ControlChannelHandle` connects to `remote_addr` with the configured transport → sends `Hello::ControlChannelHello` (bincode) → server validates token digest (`protocol::digest`, SHA256) → heartbeat loop; on demand, data channel connects → server opens a TCP pool on the service's `bind_addr` and copies bytes bidirectionally (`client.rs` `copy_bidirectional`; server pool size 8 TCP / 2 UDP).

## Key Directories

| Path | Purpose |
|---|---|
| `src/` | Library crate (`rathole`) + all logic; binary target `rathole-x` in `src/main.rs` |
| `src/platform/` | Whole-module cfg platform layer: `windows.rs` (SCM/UAC), `other.rs` (stubs), `mod.rs` facade |
| `src/transport/` | `Transport` trait + tcp / native_tls / rustls / noise / websocket impls |
| `tests/` | Integration test crate + per-transport config tomls |
| `examples/` | Config examples incl. `tls/` (test cert material!) and `systemd/` |
| `docs/` | Upstream docs + fork-owned `plan-linux-service.md` |
| `scripts/` | `release.sh` local gh release helper |
| `.github/workflows/` | CI (`rust.yml`) and release (`release.yml`) |

## Development Commands

```bash
cargo build                                    # debug, binary target/debug/rathole-x.exe
cargo run -- <subcommand>                      # e.g. cargo run -- status
cargo test --verbose                           # unit + integration (native-tls)
cargo test --lib                               # unit tests only
cargo test --test integration_test             # integration only
cargo test --verbose --no-default-features --features server,client,rustls,noise,websocket-rustls,hot-reload
cargo check --target x86_64-unknown-linux-gnu --no-default-features --features embedded   # linux cross-check
```

- No `rust-toolchain` pin; stable only.
- `cargo test` builds bin targets too — if the installed Windows service is running, it locks `target/debug/rathole-x.exe` and linking fails. Stop/uninstall the service first.
- CI runs `cargo clippy -- -D warnings` and cargo-hack feature powerset — keep the tree warning-free.

## Code Conventions & Common Patterns

- **Errors**: `anyhow::Result` everywhere; add context with `.with_context(|| format!("... {}", path.display()))`; `bail!` for user-facing failures (lowercase, no trailing period, backtick-quote names/flags).
- **Async**: tokio; shutdown via `broadcast::channel<bool>` (main → instances); per-instance updates via `mpsc::channel` of `ConfigChange`. `tokio::select!` in client/server run loops. Cancellation-safety matters in `Transport::accept`.
- **State**: `Arc<RwLock<...>>` for shared maps (server services/control channels); `OnceLock` for SCM-threaded config path.
- **Feature gating**: every feature-gated arm needs BOTH branches — `#[cfg(feature = "x")] real impl` + `#[cfg(not(feature = "x"))] crate::helper::feature_not_compile("x")` (returns `!`). `native-tls`/`rustls` mutually exclusive via `compile_error!` in `src/transport/mod.rs`.
- **Serde config** (`src/config.rs`): `deny_unknown_fields` on every struct → unknown keys are hard errors (upstream config files must still parse: new fields need `#[serde(default)]`). Secrets use `MaskedString` (Debug-safe). `Config::from_str` is custom: fills service tokens from `default_token`, validates addrs.
- **Config editing** (`src/config_edit.rs`): toml_edit `DocumentMut` preserves comments; insert via `TableLike::insert(key, value(...))`; never `std::fs::write` a config directly — use `write_atomic` (temp + rename) so the watcher never sees partial files. New-file creation prepends `CONFIG_SKELETON` manually (toml_edit re-renders standalone comments at the end otherwise).
- **Windows code**: all in `src/platform/windows.rs`, gated by module decl; lib.rs/main.rs stay cfg-free via the facade.
- **CLI**: clap 3 derive. Adding a Cli struct field breaks `tests/common/mod.rs` struct literals — update both. Nested subcommands use struct-variant form (`Config { #[clap(subcommand)] cmd: ConfigCmd }`), not tuple variants (tuple requires `Args` impl).
- **Version contract**: `version.toml` (next to each config) stamps `version = <major>` at install; `config add/set/remove` refuse on mismatch (`config_edit::check_version_compat`). **Breaking config-schema changes MUST bump the major version**; purely additive optional fields don't. Keep the stamp out of the rathole config file itself (upstream `deny_unknown_fields` would reject it).

## Important Files

| File | Why it matters |
|---|---|
| `src/protocol.rs` | Wire format — never change; `CURRENT_PROTO_VERSION`, bincode fixed-size reads |
| `src/lib.rs` | Dispatch, dual-mode fan-out, `determine_run_mode`, `os_default_config_path` |
| `src/cli.rs` | Entire CLI surface incl. hidden `--elevated-log` (global, required by UAC relay) |
| `src/config.rs` | Schema; defaults live here (`default_heartbeat_timeout` etc.) |
| `src/config_watcher.rs` | Hot reload diffing; rescan-failure semantics |
| `src/config_edit.rs` | All config write paths; version.toml policy; skeletons |
| `src/platform/windows.rs` | SCM install/uninstall, UAC relay (`relaunch_elevated_wait` + `--elevated-log` replay), ACL grants, binary self-copy, per-service `uninstall-<N>.bat` |
| `src/status.rs` | Status tree + `--json` shape |
| `build.rs` | vergen WITHOUT git feature (native libgit2 breaks MSVC; `VERGEN_GIT_*` don't exist) |
| `examples/tls/` | TLS test material; certs expire (~1y) — regenerate with `sh create_self_signed_cert.sh` under `MSYS_NO_PATHCONV=1` |
| `docs/plan-linux-service.md` | Authoritative Linux systemd plan (Chinese, frozen) |
| `scripts/release.sh` | `./scripts/release.sh <TAG>` — Windows release build + optional gh draft |

## Runtime/Tooling Preferences

- **Toolchain**: stable Rust (no pin), Windows MSVC dev environment; git-bash shell.
- **Package manager**: cargo only; lockfile committed. `[target.'cfg(windows)'.dependencies]` gates windows-service 0.8 / tracing-appender / windows-sys 0.61 (keep `Win32_System_Registry` — `SHELLEXECUTEINFOW` needs it).
- **Deps rationale**: `toml_edit` (comment-preserving edits), `dialoguer` (TTY prompts only), `serde_json` (`--json` outputs), `atty` (color/TTY gating).
- **Do not add**: native libgit2 (vergen git), a second TLS backend simultaneously, QUIC transports without feature-gating off by default (breaks `embedded`).
- **Release**: push a tag → `release.yml` cross-builds 16 targets, drafts a GitHub Release; local helper `scripts/release.sh` uses `gh` (repo `rede97/rathole-x`).

## Service Lifecycle Facts

- Multi-service model: `service install <server|client> --yes --name N` → SCM `rathole-x-<role>-<N>`, config `<config_dir>/<N>.toml`, shared binary + per-service `uninstall-<N>.bat`. Single-role configs only: `config` ops enforce the role (server config rejects client entries).
- `service start|stop|restart [--name|--all]`, `upgrade --yes` (stop all → replace shared binary → start all), `service uninstall --yes --all` (removes everything). Uninstalling an already-removed service cleans leftovers WITHOUT UAC; normal uninstall leaves the kept config user-deletable (icacls :M).
- The SCM `launch_arguments` MUST be `["service", "run", "--config", <path>]` — missing `--config` makes the service exit at startup (SCM error 1053); guarded by a unit test.
- `run_with_config` hibernates when the config is missing/invalid at startup (2s retry + "Degraded: waiting for a config update"), recovers on the next valid config — the service never dies from a bad config.

## Testing & QA

- **Integration** (`tests/integration_test.rs`): spawns echo (8080) + pingpong (8081) servers, runs rathole in-process via `rathole::run` for each of 5 transports (tcp/tls/noise/websocket/websocket_tls), asserts TCP and UDP round-trips; covers control-channel crash/restart and load phases. Ports 2333 (control) / 2334-2335 (services) / 8080-8081 must be free — a locally running rathole service breaks them.
- **Unit tests**: in-source `#[cfg(test)]` modules; scratch dirs under `std::env::temp_dir()` with `remove_dir_all` cleanup. Key coverage: config validation, watcher diffing, version policy gates, Windows file ops (binary copy, uninstall-<N>.bat, purge semantics), `determine_run_mode` table.
- **Expectations**: new observable CLI/config behavior needs a unit test (config_edit policy patterns) or an integration case; run the full `cargo test --verbose` before delivery. UI/CLI output verified by actually running the binary (`cargo run -- <cmd>` smoke), not just tests.
- Known environmental trap: TLS test certs expire; regenerate per `examples/tls` (see Important Files).
