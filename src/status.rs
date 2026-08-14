//! `rathole-x status`: show the system service state and the configuration
//! as a compact tree. `--json` emits the same information machine-readably.

use std::path::Path;

use anyhow::Result;
use serde_json::{json, Map, Value};

use crate::cli::StatusArgs;
use crate::config::{ClientConfig, Config, ServerConfig, TransportType};

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
            if t.noise.as_ref().map(|n| n.local_private_key.is_some()).unwrap_or(false) {
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
                .or_else(|| tls.trusted_root.as_ref().map(|r| format!("trusted_root {}", r)))
        }),
        TransportType::Websocket => t
            .websocket
            .as_ref()
            .map(|w| format!("tls = {}", w.tls)),
        TransportType::Tcp => None,
    }
}

fn client_node(c: &ClientConfig) -> Node {
    let mut node = Node::new(colorize("[client]", GREEN));
    node.leaf(format!("remote_addr = {}", c.remote_addr));
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
        services.leaf(label);
    }
    node.children.push(services);
    node
}

fn server_node(s: &ServerConfig) -> Node {
    let mut node = Node::new(colorize("[server]", GREEN));
    node.leaf(format!("bind_addr   = {}", s.bind_addr));
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
fn render_one(
    args: &StatusArgs,
    name: &str,
    config_path: &std::path::Path,
    service: &Option<(String, Option<u32>)>,
) -> Result<()> {
    let content = std::fs::read_to_string(config_path).ok();
    let parsed: Option<Config> = content
        .as_deref()
        .and_then(|c| toml::from_str(c).ok());

    let (auth_file_exists, auth_allows_user) = auth_info(config_path);

    if args.json {
        let mut out = Map::new();
        out.insert(
            "service".to_owned(),
            json!({
                "name": name,
                "state": service.as_ref().map(|s| s.0.clone()),
                "pid": service.as_ref().and_then(|s| s.1),
            }),
        );
        out.insert(
            "config".to_owned(),
            json!({
                "path": config_path,
                "exists": content.is_some(),
                "valid": parsed.is_some(),
                "auth": {
                    "version_file_exists": auth_file_exists,
                    "writable_by_user": auth_allows_user,
                },
            }),
        );
        out.insert(
            "client".to_owned(),
            parsed
                .as_ref()
                .and_then(|c| c.client.as_ref())
                .map(client_json)
                .unwrap_or(Value::Null),
        );
        out.insert(
            "server".to_owned(),
            parsed
                .as_ref()
                .and_then(|c| c.server.as_ref())
                .map(server_json)
                .unwrap_or(Value::Null),
        );
        println!("{}", serde_json::to_string_pretty(&Value::Object(out))?);
        return Ok(());
    }

    println!("{}", colorize(&format!("rathole-x status: {}", name), GREEN));
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
        None => println!(
            "Service: {} — {}",
            name,
            colorize("not installed", YELLOW)
        ),
    }
    println!("Config:  {}", config_path.display());
    let auth = format!(
        "version stamp: {} | writable by you: {}",
        if auth_file_exists { "yes" } else { "none" },
        if auth_allows_user { "yes" } else { "no (UAC)" }
    );
    println!("Auth:    {}", auth);
    println!();

    match &parsed {
        Some(config) => {
            let mut root = Node::new(colorize("config", GREEN));
            if let Some(c) = &config.client {
                root.children.push(client_node(c));
            }
            if let Some(s) = &config.server {
                root.children.push(server_node(s));
            }
            let mut out = String::new();
            render_tree(&root, "", true, &mut out);
            print!("{}", out);
        }
        None => {
            let hint = if content.is_none() {
                "file missing — run `rathole-x service install server|client --yes`"
            } else {
                "invalid configuration — run `rathole-x config set` or fix the file"
            };
            println!("{}", colorize(hint, YELLOW));
        }
    }
    Ok(())
}

pub fn run_status(args: &StatusArgs) -> Result<()> {
    if let Some(name) = &args.name {
        crate::config_edit::validate_service_name(name)?;
        let config_path = crate::config_edit::config_dir().join(format!("{}.toml", name));

        #[cfg(windows)]
        let role = std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|c| toml::from_str::<Config>(&c).ok())
            .map(|c| {
                if c.client.is_some() {
                    crate::config_edit::ServiceRole::Client
                } else {
                    crate::config_edit::ServiceRole::Server
                }
            });

        #[cfg(windows)]
        let service = role
            .as_ref()
            .and_then(|r| query_named_service(r.key(), name));
        #[cfg(not(windows))]
        let service: Option<(String, Option<u32>)> = None;

        return render_one(args, name, &config_path, &service);
    }

    // List mode: every installed service.
    let services = crate::config_edit::list_installed_services()?;
    if args.json {
        let arr: Vec<Value> = services
            .iter()
            .map(|(name, role)| {
                #[cfg(windows)]
                let svc = query_named_service(role.key(), name);
                #[cfg(not(windows))]
                let svc: Option<(String, Option<u32>)> = None;
                json!({
                    "name": name,
                    "role": role.key(),
                    "state": svc.as_ref().map(|s| s.0.clone()),
                    "pid": svc.as_ref().and_then(|s| s.1),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }

    println!("{}", colorize("rathole-x services", GREEN));
    println!();
    if services.is_empty() {
        println!(
            "{}",
            colorize(
                "no installed services — run `rathole-x service install server|client --yes`",
                YELLOW
            )
        );
        return Ok(());
    }
    for (name, role) in &services {
        #[cfg(windows)]
        let svc = query_named_service(role.key(), name);
        #[cfg(not(windows))]
        let svc: Option<(String, Option<u32>)> = None;
        let state = match &svc {
            Some((s, pid)) => match pid {
                Some(pid) => format!("{} (pid {})", s, pid),
                None => s.clone(),
            },
            None => "not installed".to_owned(),
        };
        let colored = match state.as_str() {
            "Running" => colorize(&state, GREEN),
            "Stopped" => colorize(&state, RED),
            _ => state,
        };
        println!("{:12} {:8} {}", name, role.key(), colored);
    }
    println!();
    println!("Run `rathole-x status --name <name>` for the full config tree.");
    Ok(())
}

