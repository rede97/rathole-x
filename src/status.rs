//! `rathole-x status`: show the system service state and the configuration
//! as a compact tree. `--json` emits the same information machine-readably.

use std::path::Path;

use anyhow::Result;
use serde_json::{json, Map, Value};

use crate::cli::StatusArgs;
use crate::config::{ClientConfig, Config, ServerConfig, TransportType};
use crate::runtime_status::{RuntimeServiceSnapshot, RuntimeSnapshot, ServerListenerState};

const GREEN: &str = "32";
const RED: &str = "31";
const YELLOW: &str = "33";
const DIM: &str = "2";

// Defaults from config.rs; non-default values are surfaced explicitly.
const DEFAULT_HEARTBEAT_TIMEOUT: u64 = 40;
const DEFAULT_RETRY_INTERVAL: u64 = 1;
const DEFAULT_HEARTBEAT_INTERVAL: u64 = 30;

struct Node {
    label: String,
    children: Vec<Node>,
}

impl Node {
    fn new(label: String) -> Node {
        Node {
            label,
            children: Vec::new(),
        }
    }

    fn leaf(&mut self, label: String) {
        self.children.push(Node::new(label));
    }
}

fn colorize(text: &str, code: &str) -> String {
    if atty::is(atty::Stream::Stdout) {
        format!("\x1b[{}m{}\x1b[0m", code, text)
    } else {
        text.to_string()
    }
}

fn render_tree(node: &Node, prefix: &str, is_last: bool, out: &mut String) {
    let branch = if is_last { "└── " } else { "├── " };
    out.push_str(prefix);
    out.push_str(branch);
    out.push_str(&node.label);
    out.push('\n');
    let child_prefix = format!("{}{}", prefix, if is_last { "    " } else { "│   " });
    for (i, child) in node.children.iter().enumerate() {
        render_tree(child, &child_prefix, i + 1 == node.children.len(), out);
    }
}

fn mask() -> &'static str {
    "••••"
}

fn transport_label(t: &crate::config::TransportConfig) -> String {
    match t.transport_type {
        TransportType::Tcp => "tcp".to_owned(),
        TransportType::Tls => "tls".to_owned(),
        TransportType::Noise => "noise".to_owned(),
        TransportType::Websocket => "websocket".to_owned(),
    }
}

fn transport_detail(t: &crate::config::TransportConfig) -> Option<String> {
    match t.transport_type {
        TransportType::Noise => {
            if t.noise
                .as_ref()
                .map(|n| n.local_private_key.is_some())
                .unwrap_or(false)
            {
                Some(format!("local key {}", mask()))
            } else if t
                .noise
                .as_ref()
                .map(|n| n.remote_public_key.is_some())
                .unwrap_or(false)
            {
                Some(format!("remote key {}", mask()))
            } else {
                None
            }
        }
        TransportType::Tls => t.tls.as_ref().and_then(|tls| {
            tls.pkcs12
                .as_ref()
                .map(|p| format!("pkcs12 {}", p))
                .or_else(|| {
                    tls.trusted_root
                        .as_ref()
                        .map(|r| format!("trusted_root {}", r))
                })
        }),
        TransportType::Websocket => t.websocket.as_ref().map(|w| format!("tls = {}", w.tls)),
        TransportType::Tcp => None,
    }
}

fn status_badge(ok: bool) -> String {
    let text = if ok { "status: ok" } else { "status: error" };
    colorize(text, if ok { GREEN } else { RED })
}

fn client_service_detail(runtime: Option<&RuntimeSnapshot>, name: &str) -> String {
    let ok = matches!(
        runtime.and_then(|snapshot| snapshot.services.get(name)),
        Some(RuntimeServiceSnapshot::Client {
            state: crate::runtime_status::ClientControlState::Connected,
            ..
        })
    );
    format!(" [{}]", status_badge(ok))
}

fn server_service_detail(runtime: Option<&RuntimeSnapshot>, name: &str) -> String {
    match runtime.and_then(|snapshot| snapshot.services.get(name)) {
        Some(RuntimeServiceSnapshot::Server {
            state: crate::runtime_status::ServerControlState::Connected,
            control_channel_source,
            ..
        }) => format!(
            " [{}]{}",
            status_badge(true),
            control_channel_source
                .map(|address| format!(" [client: {}]", address))
                .unwrap_or_default()
        ),
        _ => format!(" [{}]", status_badge(false)),
    }
}

fn client_remote_detail(runtime: Option<&RuntimeSnapshot>) -> String {
    let Some(snapshot) = runtime else {
        return format!(" [{}]", status_badge(false));
    };
    let connected = snapshot
        .services
        .values()
        .find_map(|service| match service {
            RuntimeServiceSnapshot::Client {
                state: crate::runtime_status::ClientControlState::Connected,
                resolved_control_target,
                ..
            } => *resolved_control_target,
            _ => None,
        });
    match connected {
        Some(address) => format!(" [{}] [resolved: {}]", status_badge(true), address),
        None => format!(" [{}]", status_badge(false)),
    }
}

fn server_bind_detail(runtime: Option<&RuntimeSnapshot>) -> String {
    let ok = matches!(
        runtime.and_then(|snapshot| snapshot.server_listener.as_ref()),
        Some(listener) if listener.state == ServerListenerState::Listening
    );
    format!(" [{}]", status_badge(ok))
}

fn client_node(c: &ClientConfig, runtime: Option<&RuntimeSnapshot>) -> Node {
    let mut node = Node::new(colorize("[client]", GREEN));
    node.leaf(format!(
        "remote_addr = {}{}",
        c.remote_addr,
        client_remote_detail(runtime)
    ));
    let transport = format!("transport   = {}", transport_label(&c.transport));
    node.leaf(transport);
    if let Some(detail) = transport_detail(&c.transport) {
        node.leaf(format!("transport detail = {}", colorize(&detail, DIM)));
    }
    if c.prefer_ipv6 == Some(true) {
        node.leaf("prefer_ipv6 = true".to_owned());
    }
    if c.heartbeat_timeout != DEFAULT_HEARTBEAT_TIMEOUT {
        node.leaf(format!("heartbeat_timeout = {}", c.heartbeat_timeout));
    }
    if c.retry_interval != DEFAULT_RETRY_INTERVAL {
        node.leaf(format!("retry_interval = {}", c.retry_interval));
    }
    if c.default_token.is_some() {
        node.leaf(format!("default_token = {}", mask()));
    }

    let mut services = Node::new(format!("services ({})", c.services.len()));
    for (name, s) in &c.services {
        let mut label = format!("{} → {}", name, s.local_addr);
        if s.service_type == crate::config::ServiceType::Udp {
            label.push_str(" [udp]");
        }
        if s.token.is_some() {
            label.push_str(&format!(" [token {}]", mask()));
        }
        label.push_str(&client_service_detail(runtime, name));
        services.leaf(label);
    }
    node.children.push(services);
    node
}

fn server_node(s: &ServerConfig, runtime: Option<&RuntimeSnapshot>) -> Node {
    let mut node = Node::new(colorize("[server]", GREEN));
    node.leaf(format!(
        "bind_addr   = {}{}",
        s.bind_addr,
        server_bind_detail(runtime)
    ));
    let transport = format!("transport   = {}", transport_label(&s.transport));
    node.leaf(transport);
    if let Some(detail) = transport_detail(&s.transport) {
        node.leaf(format!("transport detail = {}", colorize(&detail, DIM)));
    }
    if s.heartbeat_interval != DEFAULT_HEARTBEAT_INTERVAL {
        node.leaf(format!("heartbeat_interval = {}", s.heartbeat_interval));
    }
    if s.default_token.is_some() {
        node.leaf(format!("default_token = {}", mask()));
    }

    let mut services = Node::new(format!("services ({})", s.services.len()));
    for (name, svc) in &s.services {
        let mut label = format!("{} → {}", name, svc.bind_addr);
        if svc.service_type == crate::config::ServiceType::Udp {
            label.push_str(" [udp]");
        }
        if svc.token.is_some() {
            label.push_str(&format!(" [token {}]", mask()));
        }
        label.push_str(&server_service_detail(runtime, name));
        services.leaf(label);
    }
    node.children.push(services);
    node
}

fn auth_info(config_path: &Path) -> (bool, bool) {
    let exists = crate::config_edit::version_path(config_path).exists();
    (
        exists,
        crate::config_edit::writable_by_current_user(config_path),
    )
}

fn runtime_status_for(config_path: &Path) -> (Value, Option<RuntimeSnapshot>, Option<String>) {
    match crate::platform::query_runtime_status(config_path) {
        Ok(snapshot) => match serde_json::to_value(&snapshot) {
            Ok(value) => (value, Some(snapshot), None),
            Err(error) => (
                Value::Null,
                None,
                Some(format!("invalid local snapshot: {}", error)),
            ),
        },
        Err(error) => (
            Value::Null,
            None,
            Some(format!("local endpoint unavailable: {}", error)),
        ),
    }
}

fn print_runtime_human(runtime: Option<&RuntimeSnapshot>, unavailable: Option<&str>) {
    match runtime {
        Some(snapshot) => println!(
            "Runtime: {} (pid {}, snapshot {})",
            match snapshot.role {
                crate::runtime_status::RuntimeRole::Client => "client",
                crate::runtime_status::RuntimeRole::Server => "server",
            },
            snapshot.process_id,
            snapshot.captured_at_unix_ms
        ),
        None => println!(
            "Runtime: {}",
            colorize(
                &format!(
                    "unavailable — {}",
                    unavailable.unwrap_or("local endpoint did not return a snapshot")
                ),
                YELLOW
            )
        ),
    }
}

/// Query the Windows service state. `None` when not installed or unreadable.
#[cfg(windows)]
fn query_windows_service(name: &str) -> Option<(String, Option<u32>)> {
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).ok()?;
    let service = manager
        .open_service(name, ServiceAccess::QUERY_STATUS)
        .ok()?;
    let status = service.query_status().ok()?;
    Some((format!("{:?}", status.current_state), status.process_id))
}

fn client_json(c: &ClientConfig) -> Value {
    let services: Map<String, Value> = c
        .services
        .iter()
        .map(|(name, s)| {
            (
                name.clone(),
                json!({
                    "local_addr": s.local_addr,
                    "type": format!("{:?}", s.service_type).to_lowercase(),
                    "token_set": s.token.is_some(),
                }),
            )
        })
        .collect();
    json!({
        "remote_addr": c.remote_addr,
        "transport": transport_label(&c.transport),
        "default_token_set": c.default_token.is_some(),
        "services": services,
    })
}

fn server_json(s: &ServerConfig) -> Value {
    let services: Map<String, Value> = s
        .services
        .iter()
        .map(|(name, svc)| {
            (
                name.clone(),
                json!({
                    "bind_addr": svc.bind_addr,
                    "type": format!("{:?}", svc.service_type).to_lowercase(),
                    "token_set": svc.token.is_some(),
                }),
            )
        })
        .collect();
    json!({
        "bind_addr": s.bind_addr,
        "transport": transport_label(&s.transport),
        "default_token_set": s.default_token.is_some(),
        "services": services,
    })
}

/// SCM name for a named service role, mirrored from the platform module.
#[cfg(windows)]
fn scm_service_name(role: &str, name: &str) -> String {
    format!("rathole-x-{}-{}", role, name)
}

/// Query one installed service's SCM state.
#[cfg(windows)]
fn query_named_service(role: &str, name: &str) -> Option<(String, Option<u32>)> {
    query_windows_service(&scm_service_name(role, name))
}

/// Render the full status for one service config.
///
/// `state_known` is false when the SCM state cannot be inferred (a `-c`
/// config outside the service directory): the human output then says
/// "unknown" instead of "not installed" (JSON keeps `state: null`).
fn render_one(
    args: &StatusArgs,
    name: &str,
    config_path: &std::path::Path,
    service: &Option<(String, Option<u32>)>,
    state_known: bool,
) -> Result<Value> {
    let read_result = std::fs::read_to_string(config_path);
    let content = read_result.as_ref().ok();
    let parsed: Option<Config> = content.and_then(|c| toml::from_str(c).ok());
    let config_access = match &read_result {
        Ok(_) => "readable",
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "missing",
        Err(_) => "denied",
    };
    let (auth_file_exists, auth_allows_user) = auth_info(config_path);
    let (runtime, runtime_snapshot, runtime_unavailable) = runtime_status_for(config_path);
    let result = json!({
        "service": {
            "name": name,
            "state": service.as_ref().map(|s| s.0.clone()),
            "pid": service.as_ref().and_then(|s| s.1),
        },
        "config": {
            "path": config_path,
            "exists": content.is_some(),
            "readable": read_result.is_ok(),
            "access": config_access,
            "valid": parsed.is_some(),
            "auth": {
                "version_file_exists": auth_file_exists,
                "writable_by_user": auth_allows_user,
            },
        },
        "client": parsed.as_ref().and_then(|c| c.client.as_ref()).map(client_json),
        "server": parsed.as_ref().and_then(|c| c.server.as_ref()).map(server_json),
        "runtime": runtime,
        "runtime_availability": {
            "available": runtime_snapshot.is_some(),
            "reason": runtime_unavailable,
        },
    });
    if args.json {
        return Ok(result);
    }

    println!(
        "{}",
        colorize(&format!("rathole-x status: {}", name), GREEN)
    );
    println!();
    match service {
        Some((state, pid)) => {
            let colored = match state.as_str() {
                "Running" => colorize(state, GREEN),
                "Stopped" => colorize(state, RED),
                _ => colorize(state, YELLOW),
            };
            match pid {
                Some(pid) => println!("Service: {} — {} (pid {})", name, colored, pid),
                None => println!("Service: {} — {}", name, colored),
            }
        }
        None if !state_known => println!(
            "Service: {} — {}",
            name,
            colorize("unknown (config outside the service directory)", YELLOW)
        ),
        None => println!("Service: {} — {}", name, colorize("not installed", YELLOW)),
    }
    println!("Config:  {}", config_path.display());
    println!(
        "Auth:    version stamp: {} | writable by you: {}",
        if auth_file_exists { "yes" } else { "none" },
        if auth_allows_user { "yes" } else { "no (UAC)" }
    );
    print_runtime_human(runtime_snapshot.as_ref(), runtime_unavailable.as_deref());
    println!();
    match &parsed {
        Some(config) => {
            let mut root = Node::new(colorize("config", GREEN));
            if let Some(c) = &config.client {
                root.children
                    .push(client_node(c, runtime_snapshot.as_ref()));
            }
            if let Some(s) = &config.server {
                root.children
                    .push(server_node(s, runtime_snapshot.as_ref()));
            }
            let mut out = String::new();
            render_tree(&root, "", true, &mut out);
            print!("{}", out);
        }
        None => {
            let hint = match config_access {
                "missing" => "config file missing — run `rathole-x service install server|client --yes`",
                "denied" => "config permission denied — static config tree hidden; service and runtime state above remain readable",
                _ => "invalid configuration — run `rathole-x config set` or fix the file",
            };
            println!("{}", colorize(hint, YELLOW));
        }
    }
    Ok(result)
}

/// SCM state of the named service. The running runtime snapshot is the
/// preferred role source because the installed config may be unreadable to a
/// normal user by ACL; the readable config is the fallback.
fn service_state_for(name: &str, config_path: &Path) -> Option<(String, Option<u32>)> {
    #[cfg(windows)]
    {
        let runtime_role =
            crate::platform::query_runtime_status(config_path)
                .ok()
                .map(|snapshot| match snapshot.role {
                    crate::runtime_status::RuntimeRole::Client => {
                        crate::config_edit::ServiceRole::Client
                    }
                    crate::runtime_status::RuntimeRole::Server => {
                        crate::config_edit::ServiceRole::Server
                    }
                });
        let role = runtime_role.or_else(|| {
            std::fs::read_to_string(config_path)
                .ok()
                .and_then(|c| toml::from_str::<Config>(&c).ok())
                .map(|c| {
                    if c.client.is_some() {
                        crate::config_edit::ServiceRole::Client
                    } else {
                        crate::config_edit::ServiceRole::Server
                    }
                })
        })?;
        query_named_service(role.key(), name)
    }
    #[cfg(target_os = "linux")]
    {
        let role = std::fs::read_to_string(config_path)
            .ok()
            .and_then(|c| toml::from_str::<Config>(&c).ok())
            .map(|c| {
                if c.client.is_some() {
                    crate::config_edit::ServiceRole::Client
                } else {
                    crate::config_edit::ServiceRole::Server
                }
            })?;
        crate::platform::query_service_state(role.key(), name)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = (name, config_path);
        None
    }
}

pub fn run_status(args: &StatusArgs) -> Result<Value> {
    if let Some(name) = &args.name {
        crate::config_edit::validate_service_name(name)?;
        let config_path = match &args.config {
            Some(p) => p.clone(),
            None => crate::config_edit::config_dir().join(format!("{}.toml", name)),
        };
        let service = service_state_for(name, &config_path);
        return render_one(args, name, &config_path, &service, true);
    }
    if let Some(config_path) = &args.config {
        let name = config_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("config")
            .to_owned();
        let state_known = config_path.parent() == Some(crate::config_edit::config_dir().as_path());
        let service = if state_known {
            service_state_for(&name, config_path)
        } else {
            None
        };
        return render_one(args, &name, config_path, &service, state_known);
    }
    let services = crate::config_edit::list_installed_services()?;
    let arr: Vec<Value> = services
        .iter()
        .map(|(name, role)| {
            #[cfg(windows)]
            let svc = query_named_service(role.key(), name);
            #[cfg(target_os = "linux")]
            let svc = crate::platform::query_service_state(role.key(), name);
            #[cfg(not(any(windows, target_os = "linux")))]
            let svc: Option<(String, Option<u32>)> = None;
            let config_path = crate::config_edit::config_dir().join(format!("{}.toml", name));
            let (runtime, runtime_snapshot, runtime_unavailable) = runtime_status_for(&config_path);
            json!({
                "name": name,
                "role": role.key(),
                "state": svc.as_ref().map(|s| s.0.clone()),
                "pid": svc.as_ref().and_then(|s| s.1),
                "runtime": runtime,
                "runtime_availability": {
                    "available": runtime_snapshot.is_some(),
                    "reason": runtime_unavailable,
                },
            })
        })
        .collect();
    if !args.json {
        println!("{}", colorize("rathole-x services", GREEN));
        println!();
        if arr.is_empty() {
            println!(
                "{}",
                colorize(
                    "no installed services — run `rathole-x service install server|client --yes`",
                    YELLOW
                )
            );
        } else {
            for ((name, role), detail) in services.iter().zip(&arr) {
                #[cfg(windows)]
                let svc = query_named_service(role.key(), name);
                #[cfg(target_os = "linux")]
                let svc = crate::platform::query_service_state(role.key(), name);
                #[cfg(not(any(windows, target_os = "linux")))]
                let svc: Option<(String, Option<u32>)> = None;
                let state = match &svc {
                    Some((s, Some(pid))) => format!("{} (pid {})", s, pid),
                    Some((s, None)) => s.clone(),
                    None => "not installed".to_owned(),
                };
                let runtime = detail["runtime"]
                    .get("role")
                    .and_then(Value::as_str)
                    .map(|role| format!("runtime {}", role))
                    .unwrap_or_else(|| "runtime unavailable".to_owned());
                println!("{:12} {:8} {} | {}", name, role.key(), state, runtime);
            }
            println!();
            println!("Run `rathole-x status --name <name>` for the full config tree.");
        }
    }
    Ok(json!({"services": arr}))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENT_TOML: &str = r#"
[client]
remote_addr = "127.0.0.1:2333"

[client.services.svc]
local_addr = "127.0.0.1:8080"
token = "abc"
"#;

    /// `status -c <file>` renders the given file's config tree even when it
    /// lives outside the service config directory (the SCM state is then
    /// reported as unknown instead of "not installed").
    #[test]
    fn status_config_without_name_renders_file() {
        let dir =
            std::env::temp_dir().join(format!("rathole-x-status-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("demo.toml");
        std::fs::write(&path, CLIENT_TOML).unwrap();

        for json in [false, true] {
            let args = StatusArgs {
                config: Some(path.clone()),
                name: None,
                json,
            };
            let result = run_status(&args).unwrap();
            if json {
                assert_eq!(result["config"]["access"], "readable");
                assert!(result["runtime"].is_null());
                assert_eq!(result["runtime_availability"]["available"], false);
            }
        }

        // `--name` combined with `-c` prefers the explicit file.
        let args = StatusArgs {
            config: Some(path.clone()),
            name: Some("demo".to_owned()),
            json: true,
        };
        let result = run_status(&args).unwrap();
        assert!(result["runtime"].is_null());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn status_missing_file_reports_missing_not_denied() {
        let dir =
            std::env::temp_dir().join(format!("rathole-x-status-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("missing.toml");
        let args = StatusArgs {
            config: Some(path),
            name: None,
            json: true,
        };
        let result = run_status(&args).unwrap();
        assert_eq!(result["config"]["access"], "missing");
        assert_eq!(result["config"]["exists"], false);
        std::fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn runtime_badges_reflect_client_and_server_state() {
        let client = RuntimeSnapshot {
            schema_version: crate::runtime_status::RUNTIME_SCHEMA_VERSION,
            role: crate::runtime_status::RuntimeRole::Client,
            process_id: 1,
            captured_at_unix_ms: 1,
            server_listener: None,
            services: [(
                "demo".to_owned(),
                RuntimeServiceSnapshot::Client {
                    state: crate::runtime_status::ClientControlState::Connected,
                    configured_control_target: "example.test:2333".to_owned(),
                    resolved_control_target: Some("127.0.0.1:2333".parse().unwrap()),
                    last_connected_at_unix_ms: Some(1),
                    last_error: None,
                },
            )]
            .into_iter()
            .collect(),
        };
        assert_eq!(
            client_remote_detail(Some(&client)),
            " [status: ok] [resolved: 127.0.0.1:2333]"
        );
        assert_eq!(
            client_service_detail(Some(&client), "demo"),
            " [status: ok]"
        );

        let mut server = RuntimeSnapshot {
            schema_version: crate::runtime_status::RUNTIME_SCHEMA_VERSION,
            role: crate::runtime_status::RuntimeRole::Server,
            process_id: 2,
            captured_at_unix_ms: 2,
            server_listener: Some(crate::runtime_status::ServerListenerSnapshot {
                state: ServerListenerState::Pending,
                last_error: None,
            }),
            services: std::collections::BTreeMap::new(),
        };
        assert_eq!(server_bind_detail(Some(&server)), " [status: error]");
        server.server_listener = Some(crate::runtime_status::ServerListenerSnapshot {
            state: ServerListenerState::Listening,
            last_error: None,
        });
        assert_eq!(server_bind_detail(Some(&server)), " [status: ok]");
        assert_eq!(server_bind_detail(None), " [status: error]");
    }
}
