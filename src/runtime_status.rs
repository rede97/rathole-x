//! Local, read-only runtime status snapshots.
//!
//! This module deliberately contains no proxy protocol messages.  The pipe
//! transport is a local management plane and exposes only this small,
//! versioned snapshot.

use std::collections::BTreeMap;
use std::net::SocketAddr;
#[cfg(any(windows, target_os = "linux", test))]
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
#[cfg(any(windows, target_os = "linux", test))]
use sha2::{Digest, Sha256};

pub const RUNTIME_SCHEMA_VERSION: u32 = 2;
#[cfg(any(windows, target_os = "linux"))]
pub(crate) const STATUS_REQUEST: &[u8] = b"rathole-x-status-v1";
#[cfg(any(windows, target_os = "linux"))]
pub(crate) const MAX_STATUS_RESPONSE: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeRole {
    Client,
    Server,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientControlState {
    Connecting,
    Connected,
    Retrying,
    Stopped,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerControlState {
    Waiting,
    Connected,
    Stopped,
}

/// Lifecycle of the configured server listener, independent of service
/// control-channel registrations.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerListenerState {
    Pending,
    Listening,
    Error,
    Stopped,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServerListenerSnapshot {
    pub state: ServerListenerState,
    /// Bounded bind diagnostic. It is omitted from the human tree.
    pub last_error: Option<RuntimeEvent>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeEvent {
    pub at_unix_ms: u64,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeServiceSnapshot {
    Client {
        state: ClientControlState,
        /// Configured `client.remote_addr`, not an upstream proxy peer.
        configured_control_target: String,
        resolved_control_target: Option<SocketAddr>,
        last_connected_at_unix_ms: Option<u64>,
        last_error: Option<RuntimeEvent>,
    },
    Server {
        state: ServerControlState,
        /// The accepted authenticated control-channel source address.
        control_channel_source: Option<SocketAddr>,
        connected_since_unix_ms: Option<u64>,
        last_disconnected_or_error: Option<RuntimeEvent>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeSnapshot {
    pub schema_version: u32,
    pub role: RuntimeRole,
    pub process_id: u32,
    /// Wall-clock capture time.  It makes the age of event timestamps clear
    /// without claiming cross-process monotonicity.
    pub captured_at_unix_ms: u64,
    /// `None` for a client runtime.
    #[serde(default)]
    pub server_listener: Option<ServerListenerSnapshot>,
    pub services: BTreeMap<String, RuntimeServiceSnapshot>,
}

pub(crate) type RuntimeRegistry = Arc<RwLock<RuntimeSnapshot>>;

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn safe_error(error: impl std::fmt::Display) -> RuntimeEvent {
    // Status is metadata, not a general error/log export.  Keep a bounded
    // diagnostic so a pathological transport error cannot grow a response.
    let mut message = error.to_string();
    message.truncate(512);
    RuntimeEvent {
        at_unix_ms: now_unix_ms(),
        message,
    }
}

pub(crate) fn client_registry(
    remote_addr: &str,
    services: impl IntoIterator<Item = String>,
) -> RuntimeRegistry {
    let services = services
        .into_iter()
        .map(|name| {
            (
                name,
                RuntimeServiceSnapshot::Client {
                    state: ClientControlState::Stopped,
                    configured_control_target: remote_addr.to_owned(),
                    resolved_control_target: None,
                    last_connected_at_unix_ms: None,
                    last_error: None,
                },
            )
        })
        .collect();
    Arc::new(RwLock::new(RuntimeSnapshot {
        schema_version: RUNTIME_SCHEMA_VERSION,
        role: RuntimeRole::Client,
        process_id: std::process::id(),
        captured_at_unix_ms: now_unix_ms(),
        server_listener: None,
        services,
    }))
}

pub(crate) fn server_registry(services: impl IntoIterator<Item = String>) -> RuntimeRegistry {
    let services = services
        .into_iter()
        .map(|name| {
            (
                name,
                RuntimeServiceSnapshot::Server {
                    state: ServerControlState::Waiting,
                    control_channel_source: None,
                    connected_since_unix_ms: None,
                    last_disconnected_or_error: None,
                },
            )
        })
        .collect();
    Arc::new(RwLock::new(RuntimeSnapshot {
        schema_version: RUNTIME_SCHEMA_VERSION,
        role: RuntimeRole::Server,
        process_id: std::process::id(),
        captured_at_unix_ms: now_unix_ms(),
        server_listener: Some(ServerListenerSnapshot {
            state: ServerListenerState::Pending,
            last_error: None,
        }),
        services,
    }))
}

#[cfg(any(windows, target_os = "linux", test))]
pub(crate) fn snapshot(registry: &RuntimeRegistry) -> RuntimeSnapshot {
    let mut snapshot = registry
        .read()
        .expect("runtime status lock poisoned")
        .clone();
    snapshot.captured_at_unix_ms = now_unix_ms();
    snapshot
}

pub(crate) fn server_listener_pending(registry: &RuntimeRegistry) {
    if let Some(listener) = registry
        .write()
        .expect("runtime status lock poisoned")
        .server_listener
        .as_mut()
    {
        listener.state = ServerListenerState::Pending;
        listener.last_error = None;
    }
}

pub(crate) fn server_listener_listening(registry: &RuntimeRegistry) {
    if let Some(listener) = registry
        .write()
        .expect("runtime status lock poisoned")
        .server_listener
        .as_mut()
    {
        listener.state = ServerListenerState::Listening;
        listener.last_error = None;
    }
}

pub(crate) fn server_listener_error(registry: &RuntimeRegistry, error: impl std::fmt::Display) {
    if let Some(listener) = registry
        .write()
        .expect("runtime status lock poisoned")
        .server_listener
        .as_mut()
    {
        listener.state = ServerListenerState::Error;
        listener.last_error = Some(safe_error(error));
    }
}

pub(crate) fn server_listener_stopped(registry: &RuntimeRegistry) {
    if let Some(listener) = registry
        .write()
        .expect("runtime status lock poisoned")
        .server_listener
        .as_mut()
    {
        listener.state = ServerListenerState::Stopped;
    }
}

pub(crate) fn client_add(registry: &RuntimeRegistry, name: String, remote_addr: String) {
    registry
        .write()
        .expect("runtime status lock poisoned")
        .services
        .insert(
            name,
            RuntimeServiceSnapshot::Client {
                state: ClientControlState::Stopped,
                configured_control_target: remote_addr,
                resolved_control_target: None,
                last_connected_at_unix_ms: None,
                last_error: None,
            },
        );
}

pub(crate) fn server_add(registry: &RuntimeRegistry, name: String) {
    registry
        .write()
        .expect("runtime status lock poisoned")
        .services
        .insert(
            name,
            RuntimeServiceSnapshot::Server {
                state: ServerControlState::Waiting,
                control_channel_source: None,
                connected_since_unix_ms: None,
                last_disconnected_or_error: None,
            },
        );
}

pub(crate) fn remove(registry: &RuntimeRegistry, name: &str) {
    registry
        .write()
        .expect("runtime status lock poisoned")
        .services
        .remove(name);
}

pub(crate) fn client_connecting(registry: &RuntimeRegistry, name: &str) {
    if let Some(RuntimeServiceSnapshot::Client { state, .. }) = registry
        .write()
        .expect("runtime status lock poisoned")
        .services
        .get_mut(name)
    {
        *state = ClientControlState::Connecting;
    }
}

pub(crate) fn client_resolved(registry: &RuntimeRegistry, name: &str, addr: SocketAddr) {
    if let Some(RuntimeServiceSnapshot::Client {
        resolved_control_target,
        ..
    }) = registry
        .write()
        .expect("runtime status lock poisoned")
        .services
        .get_mut(name)
    {
        *resolved_control_target = Some(addr);
    }
}

pub(crate) fn client_connected(registry: &RuntimeRegistry, name: &str) {
    if let Some(RuntimeServiceSnapshot::Client {
        state,
        last_connected_at_unix_ms,
        last_error,
        ..
    }) = registry
        .write()
        .expect("runtime status lock poisoned")
        .services
        .get_mut(name)
    {
        *state = ClientControlState::Connected;
        *last_connected_at_unix_ms = Some(now_unix_ms());
        *last_error = None;
    }
}

pub(crate) fn client_retrying(
    registry: &RuntimeRegistry,
    name: &str,
    error: impl std::fmt::Display,
) {
    if let Some(RuntimeServiceSnapshot::Client {
        state, last_error, ..
    }) = registry
        .write()
        .expect("runtime status lock poisoned")
        .services
        .get_mut(name)
    {
        *state = ClientControlState::Retrying;
        *last_error = Some(safe_error(error));
    }
}

pub(crate) fn client_stopped(registry: &RuntimeRegistry, name: &str) {
    if let Some(RuntimeServiceSnapshot::Client { state, .. }) = registry
        .write()
        .expect("runtime status lock poisoned")
        .services
        .get_mut(name)
    {
        *state = ClientControlState::Stopped;
    }
}

pub(crate) fn server_connected(
    registry: &RuntimeRegistry,
    name: &str,
    source: SocketAddr,
) -> Option<u64> {
    let connected_since = now_unix_ms();
    if let Some(RuntimeServiceSnapshot::Server {
        state,
        control_channel_source,
        connected_since_unix_ms,
        last_disconnected_or_error,
    }) = registry
        .write()
        .expect("runtime status lock poisoned")
        .services
        .get_mut(name)
    {
        *state = ServerControlState::Connected;
        *control_channel_source = Some(source);
        *connected_since_unix_ms = Some(connected_since);
        *last_disconnected_or_error = None;
        Some(connected_since)
    } else {
        None
    }
}

/// Clear only the channel that currently owns the snapshot. An older
/// superseded task must never turn a replacement control channel into
/// `waiting`.
pub(crate) fn server_disconnected(
    registry: &RuntimeRegistry,
    name: &str,
    source: SocketAddr,
    connected_since: u64,
    detail: Option<impl std::fmt::Display>,
) {
    if let Some(RuntimeServiceSnapshot::Server {
        state,
        control_channel_source,
        connected_since_unix_ms,
        last_disconnected_or_error,
    }) = registry
        .write()
        .expect("runtime status lock poisoned")
        .services
        .get_mut(name)
    {
        if *control_channel_source != Some(source)
            || *connected_since_unix_ms != Some(connected_since)
        {
            return;
        }
        *state = ServerControlState::Waiting;
        *control_channel_source = None;
        *connected_since_unix_ms = None;
        *last_disconnected_or_error = detail.map(safe_error);
    }
}

/// Derive the local endpoint name from a canonical path without leaking that
/// path into the OS-wide endpoint namespace. Windows prefixes this with the
/// named-pipe root; Linux binds it as an abstract socket name.
#[cfg(any(windows, target_os = "linux", test))]
pub(crate) fn status_name_for_config(config_path: &Path) -> String {
    let path = canonical_config_path(config_path);
    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    format!("rathole-x-status-{}", hex::encode(&digest[..16]))
}

#[cfg(any(windows, test))]
pub(crate) fn endpoint_for_config(config_path: &Path) -> String {
    format!(r"\\.\pipe\{}", status_name_for_config(config_path))
}

#[cfg(any(windows, target_os = "linux", test))]
fn canonical_config_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_owned())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_listener_transitions_serialize_and_clear_on_restart() {
        let registry = server_registry(["demo".to_owned()]);
        let initial = snapshot(&registry);
        assert_eq!(
            initial.server_listener,
            Some(ServerListenerSnapshot {
                state: ServerListenerState::Pending,
                last_error: None,
            })
        );

        server_listener_listening(&registry);
        let listening = serde_json::to_value(snapshot(&registry)).unwrap();
        assert_eq!(listening["schema_version"], RUNTIME_SCHEMA_VERSION);
        assert_eq!(listening["server_listener"]["state"], "listening");
        assert!(listening["server_listener"]["last_error"].is_null());

        server_listener_error(&registry, "address already in use");
        let failed = serde_json::to_value(snapshot(&registry)).unwrap();
        assert_eq!(failed["server_listener"]["state"], "error");
        assert_eq!(
            failed["server_listener"]["last_error"]["message"],
            "address already in use"
        );

        server_listener_pending(&registry);
        server_listener_listening(&registry);
        server_listener_stopped(&registry);
        assert_eq!(
            snapshot(&registry).server_listener,
            Some(ServerListenerSnapshot {
                state: ServerListenerState::Stopped,
                last_error: None,
            })
        );
    }

    #[test]
    fn endpoint_is_stable_for_a_canonical_path() {
        let path = std::env::temp_dir().join("rathole-x-runtime-status.toml");
        assert_eq!(endpoint_for_config(&path), endpoint_for_config(&path));
        assert!(endpoint_for_config(&path).starts_with(r"\\.\pipe\rathole-x-status-"));
    }

    #[test]
    fn client_transitions_serialize() {
        let registry = client_registry("127.0.0.1:2333", ["demo".to_owned()]);
        client_connecting(&registry, "demo");
        client_resolved(&registry, "demo", "127.0.0.1:2333".parse().unwrap());
        client_connected(&registry, "demo");
        let value = serde_json::to_value(snapshot(&registry)).unwrap();
        assert_eq!(value["schema_version"], RUNTIME_SCHEMA_VERSION);
        assert_eq!(value["services"]["demo"]["state"], "connected");
        assert_eq!(
            value["services"]["demo"]["resolved_control_target"],
            "127.0.0.1:2333"
        );
    }

    #[test]
    fn replaced_server_channel_ignores_old_channel_teardown() {
        let registry = server_registry(["demo".to_owned()]);
        let old_source = "127.0.0.1:5000".parse().unwrap();
        let old_since = server_connected(&registry, "demo", old_source).unwrap();
        let new_source = "127.0.0.1:5001".parse().unwrap();
        let _new_since = server_connected(&registry, "demo", new_source).unwrap();
        server_disconnected(
            &registry,
            "demo",
            old_source,
            old_since,
            Some("old channel stopped"),
        );
        match &snapshot(&registry).services["demo"] {
            RuntimeServiceSnapshot::Server {
                state,
                control_channel_source,
                ..
            } => {
                assert_eq!(*state, ServerControlState::Connected);
                assert_eq!(*control_channel_source, Some(new_source));
            }
            _ => panic!("wrong runtime role"),
        }
    }

    #[test]
    fn server_state_needs_an_authenticated_registration() {
        let registry = server_registry(["demo".to_owned()]);
        assert_eq!(
            snapshot(&registry).services["demo"],
            RuntimeServiceSnapshot::Server {
                state: ServerControlState::Waiting,
                control_channel_source: None,
                connected_since_unix_ms: None,
                last_disconnected_or_error: None,
            }
        );
        let source = "127.0.0.1:5000".parse().unwrap();
        let connected_since = server_connected(&registry, "demo", source).unwrap();
        server_disconnected(
            &registry,
            "demo",
            source,
            connected_since,
            Some("control channel ended"),
        );
        match &snapshot(&registry).services["demo"] {
            RuntimeServiceSnapshot::Server {
                state,
                control_channel_source,
                ..
            } => {
                assert_eq!(*state, ServerControlState::Waiting);
                assert_eq!(*control_channel_source, None);
            }
            _ => panic!("wrong runtime role"),
        }
    }
}
