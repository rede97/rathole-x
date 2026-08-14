//! Comment/format-preserving editing of the rathole-x configuration file,
//! plus the `add`/`remove`/`list` subcommand implementations.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use rand::RngCore;
use serde_json::{json, Map, Value};
use toml_edit::{value, DocumentMut, Item, Table, TableLike};

use crate::cli::{
    AddArgs, KeypairType, ListArgs, RemoveArgs, ServiceTypeArg, SetArgs, TransportTypeArg,
};

const DEFAULT_SERVER_BIND_ADDR: &str = "0.0.0.0:2333";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ServiceSide {
    Client,
    Server,
}

impl ServiceSide {
    pub fn key(&self) -> &'static str {
        match self {
            ServiceSide::Client => "client",
            ServiceSide::Server => "server",
        }
    }
}

pub struct ServiceInfo {
    pub name: String,
    pub side: ServiceSide,
    pub json: Value,
}

/// Ensure the parent directory of `path` exists and report whether the
/// config file itself is missing. Nothing is written here: callers start
/// from an empty document for a new file and write `CONFIG_SKELETON` +
/// document in a single `write_atomic` call (see `save_with_skeleton`), so
/// the watcher never observes a partially written intermediate file.
pub fn ensure_config_skeleton(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
    }

    Ok(true)
}

/// Remove the service `name` from whichever side defines it.
/// Errors when no such service exists.
pub fn remove_service(path: &Path, name: &str) -> Result<()> {
    if !path.exists() {
        bail!("Configuration file {} does not exist", path.display());
    }
    let mut doc = load_document(path)?;

    for side in [ServiceSide::Client, ServiceSide::Server] {
        let removed = doc
            .get_mut(side.key())
            .and_then(|s| s.get_mut("services"))
            .and_then(Item::as_table_like_mut)
            .map(|services| services.remove(name).is_some())
            .unwrap_or(false);
        if removed {
            return save_document(path, &doc);
        }
    }

    bail!("No service named '{}' in {}", name, path.display());
}

/// List every service defined in the configuration file.
/// A missing file yields an empty list.
pub fn list_services(path: &Path) -> Result<Vec<ServiceInfo>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let doc = load_document(path)?;

    let mut out = Vec::new();
    for side in [ServiceSide::Client, ServiceSide::Server] {
        if let Some(services) = doc
            .get(side.key())
            .and_then(|s| s.get("services"))
            .and_then(Item::as_table_like)
        {
            for (name, item) in services.iter() {
                out.push(ServiceInfo {
                    name: name.to_string(),
                    side,
                    json: item_to_json(item),
                });
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Subcommand entry points
// ---------------------------------------------------------------------------

pub fn run_list(args: &ListArgs, path: &Path) -> Result<()> {
    let services = list_services(path)?;

    if args.json {
        let arr: Vec<Value> = services
            .iter()
            .map(|s| json!({"name": s.name, "side": s.side.key(), "service": s.json}))
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }

    if services.is_empty() {
        println!("No services defined in {}", path.display());
        return Ok(());
    }
    for s in &services {
        println!("{} service '{}':", s.side.key(), s.name);
        if let Value::Object(map) = &s.json {
            for (k, v) in map {
                // Secrets are masked in human output (like batch's `••••`);
                // --json above stays plaintext for machine consumers
                if is_sensitive_key(k) && v.is_string() {
                    println!("  {} = \"••••\"", k);
                } else {
                    println!("  {} = {}", k, Value::to_string(v));
                }
            }
        }
        println!();
    }
    Ok(())
}

pub fn run_remove(args: &RemoveArgs, path: &Path) -> Result<()> {
    let side = list_services(path)?
        .into_iter()
        .find(|s| s.name == args.entry)
        .map(|s| s.side);

    remove_service(path, &args.entry)?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"removed": args.entry, "side": side.map(|s| s.key())})
            )?
        );
    } else {
        println!(
            "Removed {} service '{}' from {}",
            side.map(|s| s.key()).unwrap_or("unknown"),
            args.entry,
            path.display()
        );
    }
    Ok(())
}

/// The role a config file (and its installed service) runs as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ServiceRole {
    Client,
    Server,
}

impl ServiceRole {
    pub fn key(&self) -> &'static str {
        match self {
            ServiceRole::Client => "client",
            ServiceRole::Server => "server",
        }
    }

    pub fn from_cli(role: crate::cli::RoleArg) -> ServiceRole {
        match role {
            crate::cli::RoleArg::Client => ServiceRole::Client,
            crate::cli::RoleArg::Server => ServiceRole::Server,
        }
    }
}

/// Which role a document declares, from its [client]/[server] section.
/// A config with both sections is rejected: installed services are
/// single-role; dual-section files are only for manual `run -c` use.
pub fn detect_role(doc: &DocumentMut) -> Result<ServiceRole> {
    match (doc.get("client").is_some(), doc.get("server").is_some()) {
        (true, false) => Ok(ServiceRole::Client),
        (false, true) => Ok(ServiceRole::Server),
        (true, true) => bail!(
            "The config defines both [client] and [server]; this command edits \
             single-role configs. Split it into two files and install two services."
        ),
        (false, false) => bail!("The config defines neither [client] nor [server]"),
    }
}

/// Directory that holds every installed service config (+ version.toml).
pub fn config_dir() -> PathBuf {
    crate::os_default_config_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Validate a service short name: alphanumerics, dash, underscore only.
pub fn validate_service_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!(
            "Invalid service name '{}': use letters, digits, '-' or '_' only",
            name
        );
    }
    Ok(())
}

/// Resolve the (name, config path) of an installed service. `name` defaults
/// to "default".
pub fn service_config_path(name: Option<&str>) -> Result<(String, PathBuf)> {
    let name = name.unwrap_or("default").to_string();
    validate_service_name(&name)?;
    Ok((name.clone(), config_dir().join(format!("{}.toml", name))))
}

/// Config files of installed services, as (name, role) pairs. version.toml and
/// non-toml files are ignored.
pub fn list_installed_services() -> Result<Vec<(String, ServiceRole)>> {
    let dir = config_dir();
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(out),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(stem) => stem,
            None => continue,
        };
        if stem == "version" {
            continue;
        }
        let doc = match load_document(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if let Ok(role) = detect_role(&doc) {
            out.push((stem.to_string(), role));
        }
    }
    out.sort();
    Ok(out)
}

/// Resolve which config a config command targets: explicit -c wins, then
/// --name, then the single installed service (error otherwise).
pub fn resolve_service_config(
    config: Option<&PathBuf>,
    name: Option<&str>,
) -> Result<PathBuf> {
    if let Some(c) = config {
        return Ok(c.clone());
    }
    if let Some(n) = name {
        validate_service_name(n)?;
        return Ok(config_dir().join(format!("{}.toml", n)));
    }
    let services = list_installed_services()?;
    match services.as_slice() {
        [(only, _)] => Ok(config_dir().join(format!("{}.toml", only))),
        [] => bail!(
            "No installed service found in {}: install one first \
             (`rathole-x service install server|client --yes`) or pass -c/--name",
            config_dir().display()
        ),
        many => bail!(
            "{} services installed ({}); specify one with --name",
            many.len(),
            many.iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Server config written by `service install server`.
pub const DEFAULT_SERVER_CONFIG: &str = r#"# rathole-x server configuration
# Generated by `rathole-x service install server`. Edit freely; the service
# hot-reloads changes.
#
# Add services with:
#   rathole-x config add --name <name> --server "name:...;bind:0.0.0.0:PORT"
# Tune the control channel with:
#   rathole-x config set --name <name> --server --transport noise

[server]
bind_addr = "0.0.0.0:2333"

[server.services]
"#;

/// Client config written by `service install client`.
pub const DEFAULT_CLIENT_CONFIG: &str = r#"# rathole-x client configuration
# Generated by `rathole-x service install client`. Edit freely; the service
# hot-reloads changes.
#
# Point this client at your server first:
#   rathole-x config set --name <name> --client --remote-addr <SERVER>:2333
# Then add services:
#   rathole-x config add --name <name> --client "server:<SERVER>:2333;name:...;local:127.0.0.1:PORT"

[client]
remote_addr = "example.com:2333"  # placeholder; unused until services exist

[client.services]
"#;

/// Make the config path usable for `install`: write the role skeleton when
/// the file is missing, otherwise verify it parses. Returns `true` when the
/// config was created.
pub fn ensure_role_config(path: &Path, role: ServiceRole) -> Result<bool> {
    if path.exists() {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file {}", path.display()))?;
        // Full runtime validation, not just serde parsing: a config missing
        // tokens/pkcs12 must fail here, not at service start
        crate::config::Config::validate(&content).with_context(|| {
            format!(
                "Config file {} is not a valid rathole-x configuration",
                path.display()
            )
        })?;
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create config directory {}", parent.display())
            })?;
        }
    }
    let skeleton = match role {
        ServiceRole::Client => DEFAULT_CLIENT_CONFIG,
        ServiceRole::Server => DEFAULT_SERVER_CONFIG,
    };
    write_atomic(path, skeleton)
        .with_context(|| format!("Failed to write default config {}", path.display()))?;
    Ok(true)
}





/// Atomically replace `path`: write a unique temp file in the same
/// directory, then rename it over the target. Prevents the config watcher
/// from observing a partially written file.
fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, content)
        .with_context(|| format!("Failed to write {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        // Never leave a temp file with plaintext secrets behind
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| {
            format!(
                "Failed to replace {} with {}",
                path.display(),
                tmp.display()
            )
        });
    }
    Ok(())
}

/// Version stamp for the service config, stored in `version.toml` next to
/// the config file. Written by `install`; changing it requires reinstalling
/// the service. There is deliberately no permission policy file: whether the
/// CLI elevates is decided by probing the config's actual writability.
#[derive(serde::Deserialize)]
struct VersionConfig {
    #[serde(default)]
    version: Option<u32>,
}

/// Path of the version stamp file next to a config file.
pub fn version_path(config_path: &Path) -> PathBuf {
    match config_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join("version.toml"),
        _ => PathBuf::from("version.toml"),
    }
}

/// Whether the current process can write `path` directly (no elevation).
///
/// Existing files are probed by opening for write; a missing file is probed
/// via its directory (create + delete a probe file). Pure permission
/// judgment — no policy file involved.
pub fn writable_by_current_user(path: &Path) -> bool {
    if path.exists() {
        return std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .is_ok();
    }
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => {
            let probe = parent.join(format!(".rathole-x-probe-{}", std::process::id()));
            let ok = std::fs::write(&probe, b"").is_ok();
            let _ = std::fs::remove_file(&probe);
            ok
        }
        _ => true, // relative bare file: current dir, assume writable
    }
}

/// Refuse config modifications when the version stamp was written by a
/// different major version. No stamp file (user-managed config) or a missing
/// version field (legacy) means no restriction.
pub fn check_version_compat(path: &Path) -> Result<()> {
    let vp = version_path(path);
    let content = match std::fs::read_to_string(&vp) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    let vcfg: VersionConfig = match toml::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    if let Some(stamped) = vcfg.version {
        if stamped != crate::cli::major_version() {
            bail!(
                "This config is managed by rathole-x v{} but this CLI is v{}. \
                 Reinstall the service to upgrade: `rathole-x service uninstall --yes` then \
                 `rathole-x service install <server|client> --yes`.",
                stamped,
                crate::cli::major_version()
            );
        }
    }
    Ok(())
}

pub fn run_set(args: &SetArgs, path: &Path) -> Result<()> {
    // Side selection
    let side = match (args.client, args.server) {
        (true, false) => ServiceSide::Client,
        (false, true) => ServiceSide::Server,
        (true, true) => bail!("--client and --server are mutually exclusive"),
        (false, false) => bail!("Exactly one of --client or --server is required"),
    };

    // At least one actionable flag
    let any_field = args.remote_addr.is_some()
        || args.bind_addr.is_some()
        || args.default_token.is_some()
        || args.prefer_ipv6.is_some()
        || args.heartbeat_timeout.is_some()
        || args.retry_interval.is_some()
        || args.heartbeat_interval.is_some()
        || args.transport.is_some()
        || args.noise
        || args.noise_key.is_some()
        || args.trusted_root.is_some()
        || args.hostname.is_some()
        || args.pkcs12.is_some()
        || args.pkcs12_password.is_some()
        || args.ws_tls.is_some()
        || args.nodelay.is_some()
        || args.keepalive_secs.is_some()
        || args.keepalive_interval.is_some()
        || args.proxy.is_some();
    if !any_field {
        bail!("No fields given; see `rathole-x config set --help`");
    }

    // Side-specific validation
    match side {
        ServiceSide::Client => {
            if args.bind_addr.is_some() {
                bail!("--bind-addr is a [server] field");
            }
            if args.heartbeat_interval.is_some() {
                bail!("--heartbeat-interval is a [server] field");
            }
            if args.pkcs12.is_some() || args.pkcs12_password.is_some() {
                bail!("--pkcs12/--pkcs12-password are [server] fields");
            }
            if args.noise {
                bail!(
                    "--noise (keypair generation) is server-side; \
                     clients set --noise-key <SERVER_PUBLIC_KEY>"
                );
            }
        }
        ServiceSide::Server => {
            if args.remote_addr.is_some() {
                bail!("--remote-addr is a [client] field");
            }
            if args.prefer_ipv6.is_some() {
                bail!("--prefer-ipv6 is a [client] field");
            }
            if args.heartbeat_timeout.is_some() {
                bail!("--heartbeat-timeout is a [client] field");
            }
            if args.retry_interval.is_some() {
                bail!("--retry-interval is a [client] field");
            }
            if args.trusted_root.is_some() || args.hostname.is_some() {
                bail!("--trusted-root/--hostname are [client] fields");
            }
            if args.noise_key.is_some() {
                bail!("--noise-key is a [client] field; servers use --noise");
            }
        }
    }

    // Value validation
    if let Some(a) = &args.remote_addr {
        validate_host_port(a, "--remote-addr")?;
    }
    if let Some(a) = &args.bind_addr {
        validate_host_port(a, "--bind-addr")?;
    }
    if let Some(p) = &args.proxy {
        p.parse::<url::Url>()
            .with_context(|| format!("Invalid --proxy URL '{}'", p))?;
    }

    let created = ensure_config_skeleton(path)?;
    let mut doc = if created {
        DocumentMut::new()
    } else {
        load_document(path)?
    };

    ensure_role_allows(&doc, side)?;

    let section_key = side.key();
    if doc.get(section_key).is_none() {
        // A freshly created section needs its control-channel address
        let mut section = Table::new();
        match side {
            ServiceSide::Client => {
                let remote = args.remote_addr.as_ref().ok_or_else(|| {
                    anyhow!("creating a new [client] section requires --remote-addr")
                })?;
                section.insert("remote_addr", value(remote.as_str()));
            }
            ServiceSide::Server => {
                let bind = args
                    .bind_addr
                    .as_ref()
                    .ok_or_else(|| anyhow!("creating a new [server] section requires --bind-addr"))?;
                section.insert("bind_addr", value(bind.as_str()));
            }
        }
        // `services` has no serde default; an explicit (possibly empty)
        // table is required for the section to parse.
        section.insert("services", Item::Table(Table::new()));
        doc[section_key] = Item::Table(section);
    }

    let section = doc[section_key]
        .as_table_like_mut()
        .ok_or_else(|| anyhow!("[{}] is not a table in {}", section_key, path.display()))?;

    // Scalar section fields
    if let Some(a) = &args.remote_addr {
        section.insert("remote_addr", value(a.as_str()));
    }
    if let Some(a) = &args.bind_addr {
        section.insert("bind_addr", value(a.as_str()));
    }
    if let Some(t) = &args.default_token {
        section.insert("default_token", value(t.as_str()));
    }
    if let Some(v) = args.prefer_ipv6 {
        section.insert("prefer_ipv6", value(v));
    }
    if let Some(v) = args.heartbeat_timeout {
        section.insert("heartbeat_timeout", value(i64::try_from(v)?));
    }
    if let Some(v) = args.retry_interval {
        section.insert("retry_interval", value(i64::try_from(v)?));
    }
    if let Some(v) = args.heartbeat_interval {
        section.insert("heartbeat_interval", value(i64::try_from(v)?));
    }

    // Transport subtable
    let transport_requested = args.transport.is_some()
        || args.noise
        || args.noise_key.is_some()
        || args.trusted_root.is_some()
        || args.hostname.is_some()
        || args.pkcs12.is_some()
        || args.pkcs12_password.is_some()
        || args.ws_tls.is_some()
        || args.nodelay.is_some()
        || args.keepalive_secs.is_some()
        || args.keepalive_interval.is_some()
        || args.proxy.is_some();

    let mut generated_noise: Option<(String, String)> = None;

    if transport_requested {
        let transport = ensure_table(section, "transport", path)?;

        // Explicit --transport wins; noise/tls/websocket flags imply their type
        let implied_type = if args.noise || args.noise_key.is_some() {
            Some(TransportTypeArg::Noise)
        } else if args.trusted_root.is_some()
            || args.hostname.is_some()
            || args.pkcs12.is_some()
            || args.pkcs12_password.is_some()
        {
            Some(TransportTypeArg::Tls)
        } else if args.ws_tls.is_some() {
            Some(TransportTypeArg::Websocket)
        } else {
            None
        };
        if let Some(t) = args.transport.or(implied_type) {
            transport.insert("type", value(t.key()));
        }
        // The schema has no serde default for `type`: a transport table
        // created only for tcp sub-keys (--nodelay/--keepalive/--proxy)
        // would otherwise fail validation. Preserve an existing type.
        if transport.get("type").is_none() {
            transport.insert("type", value("tcp"));
        }

        if args.noise {
            let (private_key, public_key) = crate::generate_keypair(KeypairType::X25519)?;
            let noise = ensure_table(transport, "noise", path)?;
            noise.insert("local_private_key", value(private_key.as_str()));
            generated_noise = Some((private_key, public_key));
        }
        if let Some(pub_key) = &args.noise_key {
            let noise = ensure_table(transport, "noise", path)?;
            noise.insert("remote_public_key", value(pub_key.as_str()));
        }
        if args.trusted_root.is_some()
            || args.hostname.is_some()
            || args.pkcs12.is_some()
            || args.pkcs12_password.is_some()
        {
            let tls = ensure_table(transport, "tls", path)?;
            if let Some(v) = &args.trusted_root {
                tls.insert("trusted_root", value(v.as_str()));
            }
            if let Some(v) = &args.hostname {
                tls.insert("hostname", value(v.as_str()));
            }
            if let Some(v) = &args.pkcs12 {
                tls.insert("pkcs12", value(v.as_str()));
            }
            if let Some(v) = &args.pkcs12_password {
                tls.insert("pkcs12_password", value(v.as_str()));
            }
        }
        if let Some(b) = args.ws_tls {
            let ws = ensure_table(transport, "websocket", path)?;
            ws.insert("tls", value(b));
        }
        if args.nodelay.is_some()
            || args.keepalive_secs.is_some()
            || args.keepalive_interval.is_some()
            || args.proxy.is_some()
        {
            let tcp = ensure_table(transport, "tcp", path)?;
            if let Some(v) = args.nodelay {
                tcp.insert("nodelay", value(v));
            }
            if let Some(v) = args.keepalive_secs {
                tcp.insert("keepalive_secs", value(i64::try_from(v)?));
            }
            if let Some(v) = args.keepalive_interval {
                tcp.insert("keepalive_interval", value(i64::try_from(v)?));
            }
            if let Some(v) = &args.proxy {
                tcp.insert("proxy", value(v.as_str()));
            }
        }
    }

    // Refuse to save a configuration the runtime loader would reject
    // (missing token, tls without pkcs12, ...)
    crate::config::Config::validate(&doc.to_string()).with_context(|| {
        format!(
            "Refusing to save an invalid configuration to {}",
            path.display()
        )
    })?;

    // Save, prepending the skeleton when the file was just created
    if created {
        write_atomic(path, &format!("{}\n{}", CONFIG_SKELETON, doc))
            .with_context(|| format!("Failed to write {}", path.display()))?;
    } else {
        save_document(path, &doc)?;
    }

    // Output
    let section_json = item_to_json(&doc[section_key]);
    if args.json {
        let generated = match &generated_noise {
            Some((private, public)) => json!({
                "noise_private_key": private,
                "noise_public_key": public,
            }),
            None => json!({}),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "side": side.key(),
                "section": section_json,
                "generated": generated,
                "created": created,
            }))?
        );
    } else {
        println!("Updated [{}] in {}", section_key, path.display());
        if let Some((_, public)) = &generated_noise {
            println!("Noise public key (share with clients): {}", public);
        }
    }

    Ok(())
}

/// Get or create a nested table under `parent`, erroring when a non-table
/// value occupies the key.
fn ensure_table<'a>(
    parent: &'a mut dyn TableLike,
    key: &str,
    path: &Path,
) -> Result<&'a mut Table> {
    if parent.get(key).is_none() {
        parent.insert(key, Item::Table(Table::new()));
    }
    parent
        .get_mut(key)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| anyhow!("[{}] is not a table in {}", key, path.display()))
}

enum NoisePlan {
    None,
    Server {
        private_key: String,
        public_key: String,
    },
    Client {
        remote_public_key: String,
    },
}

pub fn run_add(args: &AddArgs, path: &Path) -> Result<()> {
    if !args.client_specs.is_empty() || !args.server_specs.is_empty() {
        return run_add_batch(args, path);
    }
    let interactive = !args.yes && atty::is(atty::Stream::Stdin) && atty::is(atty::Stream::Stdout);
    match &args.entry {
        None if interactive => run_add_interactive(args, path),
        None => bail!(
            "A service NAME is required, or pass --client/--server SPEC entries, \
             or run interactively in a TTY without a name."
        ),
        Some(_) => run_add_single(args, path),
    }
}

/// Classic mode: exactly one service from flags (+ prompts when interactive).
fn run_add_single(args: &AddArgs, path: &Path) -> Result<()> {
    let interactive = !args.yes && atty::is(atty::Stream::Stdin) && atty::is(atty::Stream::Stdout);

    // Side: inferred from the address flags, else asked interactively.
    let client_hint = args.local_addr.is_some()
        || args.remote_addr.is_some()
        || args.noise_key.is_some();
    let server_hint = args.bind_addr.is_some();
    let side = match (client_hint, server_hint) {
        (true, true) => bail!(
            "--bind-addr (server) cannot be combined with --local-addr/--remote-addr (client)"
        ),
        (true, false) => ServiceSide::Client,
        (false, true) => ServiceSide::Server,
        (false, false) => {
            if interactive {
                let choice = prompt("Side: 1=client, 2=server", Some("1"))?;
                if choice.trim() == "2" {
                    ServiceSide::Server
                } else {
                    ServiceSide::Client
                }
            } else {
                bail!(
                    "Cannot determine the side: give --local-addr/--remote-addr (client) \
                     or --bind-addr (server)"
                );
            }
        }
    };
    let name = args.entry.clone().expect("entry checked by run_add");

    // Peek at the existing configuration (if any) to learn section defaults
    let existing_doc = load_document(path).ok();
    let existing_remote_addr = existing_doc
        .as_ref()
        .and_then(|d| d.get("client"))
        .and_then(|c| c.get("remote_addr"))
        .and_then(Item::as_str)
        .map(str::to_string);

    // Resolve every required parameter: flag > existing config > prompt > error
    let mut missing: Vec<&'static str> = Vec::new();

    let remote_addr = if side == ServiceSide::Client {
        match (args.remote_addr.clone(), existing_remote_addr) {
            // One client config connects to exactly one server, like
            // upstream; a different server means a separate service instance
            (Some(flag), Some(existing)) if flag != existing => bail!(
                "this config already connects to `{}`; to use `{}`, install a separate \
                 service instance: `service install client --name <name>`",
                existing,
                flag
            ),
            (Some(flag), _) => Some(flag),
            (None, Some(existing)) => Some(existing),
            (None, None) if interactive => Some(prompt(
                "Remote address (server control channel, host:port)",
                Some("127.0.0.1:2333"),
            )?),
            (None, None) => {
                missing.push("--remote-addr");
                None
            }
        }
    } else {
        None
    };

    let local_addr = if side == ServiceSide::Client {
        match args.local_addr.clone() {
            Some(a) => Some(a),
            None if interactive => Some(prompt(
                "Local address to forward to (host:port)",
                Some("127.0.0.1:8080"),
            )?),
            None => {
                missing.push("--local-addr");
                None
            }
        }
    } else {
        None
    };

    let bind_addr = if side == ServiceSide::Server {
        match args.bind_addr.clone() {
            Some(a) => Some(a),
            None if interactive => Some(prompt(
                "Bind address of the service (host:port)",
                Some("0.0.0.0:8080"),
            )?),
            None => {
                missing.push("--bind-addr");
                None
            }
        }
    } else {
        None
    };

    let noise_requested =
        args.noise || (side == ServiceSide::Client && args.noise_key.is_some());
    let noise_key = if noise_requested && side == ServiceSide::Client {
        match args.noise_key.clone() {
            Some(k) => Some(k),
            None if interactive => Some(prompt("Server noise public key", None)?),
            None => {
                missing.push("--noise-key");
                None
            }
        }
    } else {
        None
    };

    if !missing.is_empty() {
        bail!(
            "Missing required argument(s): {}.\nThey could not be prompted for because \
             stdin/stdout is not a TTY or --yes was given.",
            missing.join(", ")
        );
    }

    // Validation
    if let Some(a) = &remote_addr {
        validate_host_port(a, "--remote-addr")?;
    }
    if let Some(a) = &local_addr {
        validate_host_port(a, "--local-addr")?;
    }
    if let Some(a) = &bind_addr {
        validate_host_port(a, "--bind-addr")?;
    }

    // Token: flag or auto-generated (never required)
    let generated_token = args.token.is_none().then(generate_token);
    let token = args
        .token
        .clone()
        .or_else(|| generated_token.clone())
        .unwrap();

    // Noise keys
    let noise_plan = match (side, noise_requested) {
        (ServiceSide::Server, true) => {
            let (private_key, public_key) = crate::generate_keypair(KeypairType::X25519)?;
            NoisePlan::Server {
                private_key,
                public_key,
            }
        }
        (ServiceSide::Client, true) => NoisePlan::Client {
            remote_public_key: noise_key.unwrap(),
        },
        _ => NoisePlan::None,
    };

    // Nothing has touched the disk yet. For a new file the document starts
    // empty and the skeleton is prepended in the single atomic save at the
    // end (a toml_edit round-trip would re-render a comment-only skeleton
    // at the end of the document).
    let created = ensure_config_skeleton(path)?;
    let mut doc = if created {
        DocumentMut::new()
    } else {
        load_document(path)?
    };

    ensure_role_allows(&doc, side)?;

    // Reject a duplicate name before ANY write: a failed `add` must be
    // side-effect free (no rotated noise key, no leftover section)
    if service_exists(&doc, side, &name) {
        bail!(
            "a {} service named `{}` already exists in {}",
            side.key(),
            name,
            path.display()
        );
    }

    let section_key = side.key();
    if doc.get(section_key).is_none() {
        let mut section = Table::new();
        match side {
            ServiceSide::Client => {
                // remote_addr was resolved above; it is required exactly when
                // the section did not already provide one
                section.insert("remote_addr", value(remote_addr.clone().unwrap().as_str()));
            }
            ServiceSide::Server => {
                section.insert("bind_addr", value(DEFAULT_SERVER_BIND_ADDR));
            }
        }
        section.insert("services", Item::Table(Table::new()));
        doc[section_key] = Item::Table(section);
    } else {
        // A pre-existing section might lack the services table
        let section = doc[section_key]
            .as_table_like_mut()
            .ok_or_else(|| anyhow!("[{}] is not a table in {}", section_key, path.display()))?;
        if section.get("services").is_none() {
            section.insert("services", Item::Table(Table::new()));
        }
    }
    apply_noise_transport(&mut doc, path, &noise_plan)?;

    // The per-service entry. Only non-default keys are written; serde
    // defaults in config.rs cover the rest (heartbeat, nodelay, ...).
    let mut entry = Table::new();
    if args.service_type == ServiceTypeArg::Udp {
        entry.insert("type", value("udp"));
    }
    match side {
        ServiceSide::Client => {
            entry.insert("local_addr", value(local_addr.unwrap().as_str()));
        }
        ServiceSide::Server => {
            entry.insert("bind_addr", value(bind_addr.unwrap().as_str()));
        }
    }
    entry.insert("token", value(token.as_str()));

    let result = insert_pending(
        &mut doc,
        PendingService {
            side,
            name: name.clone(),
            entry,
            generated_token: generated_token.clone(),
        },
    )?;
    let service_json = result["service"].clone();
    save_with_skeleton(path, &doc, created)?;

    // Output
    let generated = build_generated(&generated_token, &noise_plan);
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "name": name,
                "side": side.key(),
                "service": service_json,
                "generated": generated,
            }))?
        );
    } else {
        print_add_human(path, side, &name, &service_json, &generated_token, &noise_plan, created);
    }
    Ok(())
}

/// A parsed --client/--server SPEC entry.
#[derive(Debug, Default)]
struct ServiceSpec {
    name: String,
    remote_addr: Option<String>,
    local_addr: Option<String>,
    bind_addr: Option<String>,
    token: Option<String>,
    udp: bool,
}

/// Refuse edits that contradict the config's declared role. New/empty
/// documents (no section yet) are unrestricted: the first edit fixes the
/// role.
fn ensure_role_allows(doc: &DocumentMut, requested: ServiceSide) -> Result<()> {
    if doc.get("client").is_none() && doc.get("server").is_none() {
        return Ok(());
    }
    let role = detect_role(doc)?;
    let ok = matches!(
        (role, requested),
        (ServiceRole::Client, ServiceSide::Client)
            | (ServiceRole::Server, ServiceSide::Server)
    );
    if !ok {
        bail!(
            "This config is a {} config; {} entries do not belong here. \
             Use the matching side or another service config.",
            role.key(),
            requested.key()
        );
    }
    Ok(())
}

/// Parse one compact spec: "key:value;key:value;...".
/// Client keys: name, server, local, token, type.
/// Server keys: name, bind, token, type.
fn parse_spec(spec: &str, side: ServiceSide) -> Result<ServiceSpec> {
    let mut out = ServiceSpec::default();
    for pair in spec.split(';') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let (key, val) = pair
            .split_once(':')
            .ok_or_else(|| anyhow!("Invalid SPEC entry '{}': expected key:value", pair))?;
        let (key, val) = (key.trim(), val.trim());
        match key {
            "name" => out.name = val.to_string(),
            "token" => out.token = Some(val.to_string()),
            "type" => match val {
                "tcp" => {}
                "udp" => out.udp = true,
                other => bail!("Invalid type '{}' in SPEC (tcp|udp)", other),
            },
            "server" if side == ServiceSide::Client => {
                validate_host_port(val, "server")?;
                out.remote_addr = Some(val.to_string());
            }
            "local" if side == ServiceSide::Client => {
                validate_host_port(val, "local")?;
                out.local_addr = Some(val.to_string());
            }
            "bind" if side == ServiceSide::Server => {
                validate_host_port(val, "bind")?;
                out.bind_addr = Some(val.to_string());
            }
            other => bail!("Unknown key '{}' in {} SPEC", other, side.key()),
        }
    }
    if out.name.is_empty() {
        bail!("Missing 'name' in {} SPEC '{}'", side.key(), spec);
    }
    if side == ServiceSide::Client && out.local_addr.is_none() {
        bail!("Missing 'local' in client SPEC '{}'", spec);
    }
    if side == ServiceSide::Server && out.bind_addr.is_none() {
        bail!("Missing 'bind' in server SPEC '{}'", spec);
    }
    Ok(out)
}

/// One service ready to be written, shared by batch and interactive modes.
struct PendingService {
    side: ServiceSide,
    name: String,
    entry: Table,
    generated_token: Option<String>,
}

/// Batch mode: every --client/--server SPEC becomes a service; the whole
/// batch is written in one atomic save (one hot-reload cycle).
fn run_add_batch(args: &AddArgs, path: &Path) -> Result<()> {
    // Parse everything first: fail fast without touching the file.
    // One client config connects to exactly one server (upstream model):
    // every `server:` key in the batch must agree.
    let mut pending: Vec<PendingService> = Vec::new();
    let mut batch_remote: Option<String> = None;
    for spec in &args.client_specs {
        let s = parse_spec(spec, ServiceSide::Client)?;
        if let Some(addr) = &s.remote_addr {
            match &batch_remote {
                None => batch_remote = Some(addr.clone()),
                Some(first) if first != addr => bail!(
                    "conflicting servers `{}` and `{}` in one batch: one client config \
                     connects to one server; use separate service instances for multiple servers",
                    first,
                    addr
                ),
                _ => {}
            }
        }
        pending.push(spec_to_pending(s, ServiceSide::Client)?);
    }
    for spec in &args.server_specs {
        let s = parse_spec(spec, ServiceSide::Server)?;
        pending.push(spec_to_pending(s, ServiceSide::Server)?);
    }

    // Reject duplicate names inside the batch itself before touching the file
    let mut seen = std::collections::HashSet::new();
    for p in &pending {
        if !seen.insert((p.side, p.name.as_str())) {
            bail!(
                "duplicate {} service `{}` in this batch",
                p.side.key(),
                p.name
            );
        }
    }

    let created = ensure_config_skeleton(path)?;
    let mut doc = if created {
        DocumentMut::new()
    } else {
        load_document(path)?
    };

    let section_remote = doc
        .get("client")
        .and_then(|c| c.get("remote_addr"))
        .and_then(Item::as_str)
        .map(str::to_string);

    // The batch's server must match the one this config already connects to
    if let (Some(existing), Some(want)) = (&section_remote, &batch_remote) {
        if existing != want {
            bail!(
                "this config already connects to `{}`; to use `{}`, install a separate \
                 service instance: `service install client --name <name>`",
                existing,
                want
            );
        }
    }

    // Role gate: batch entries must match the config's declared role.
    if pending.iter().any(|p| p.side == ServiceSide::Client) {
        ensure_role_allows(&doc, ServiceSide::Client)?;
    }
    if pending.iter().any(|p| p.side == ServiceSide::Server) {
        ensure_role_allows(&doc, ServiceSide::Server)?;
    }

    // Ensure the sections exist.
    if pending.iter().any(|p| p.side == ServiceSide::Client) && doc.get("client").is_none() {
        let mut section = Table::new();
        let remote = batch_remote
            .clone()
            .or(section_remote)
            .ok_or_else(|| {
                anyhow!("Creating [client] requires a 'server:' key in a --client SPEC")
            })?;
        section.insert("remote_addr", value(remote.as_str()));
        section.insert("services", Item::Table(Table::new()));
        doc["client"] = Item::Table(section);
    }
    if pending.iter().any(|p| p.side == ServiceSide::Server) && doc.get("server").is_none() {
        let mut section = Table::new();
        section.insert("bind_addr", value(DEFAULT_SERVER_BIND_ADDR));
        section.insert("services", Item::Table(Table::new()));
        doc["server"] = Item::Table(section);
    }

    // Duplicate-name check across the batch and the existing config.
    for p in &pending {
        if service_exists(&doc, p.side, &p.name) {
            bail!("A {} service named '{}' already exists", p.side.key(), p.name);
        }
    }

    let mut results: Vec<Value> = Vec::new();
    for p in pending {
        results.push(insert_pending(&mut doc, p)?);
    }

    save_with_skeleton(path, &doc, created)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&json!(results))?);
    } else {
        for r in &results {
            let token_note = if r["service"]["token"].is_string() {
                "token ••••"
            } else {
                ""
            };
            println!(
                "Added {} service '{}' {}",
                r["side"].as_str().unwrap_or_default(),
                r["name"].as_str().unwrap_or_default(),
                token_note
            );
        }
    }
    Ok(())
}

/// Interactive multi-round mode: keep asking until an empty name ends the
/// session; everything is written in one atomic save.
fn run_add_interactive(_args: &AddArgs, path: &Path) -> Result<()> {
    let created = ensure_config_skeleton(path)?;
    let mut doc = if created {
        DocumentMut::new()
    } else {
        load_document(path)?
    };
    let existing_remote = doc
        .get("client")
        .and_then(|c| c.get("remote_addr"))
        .and_then(Item::as_str)
        .map(str::to_string);

    let mut pending: Vec<PendingService> = Vec::new();
    loop {
        let name = prompt("Service name (empty to finish)", Some(""))?;
        let name = name.trim().to_string();
        if name.is_empty() {
            break;
        }
        let side = match prompt_select(
            "Side",
            &[
                "client (forward a local port to the server)",
                "server (publish a port for clients)",
            ],
        )? {
            1 => ServiceSide::Server,
            _ => ServiceSide::Client,
        };
        if service_exists(&doc, side, &name) {
            println!("A {} service named '{}' already exists. Skipping.", side.key(), name);
            continue;
        }
        if pending.iter().any(|p| p.side == side && p.name == name) {
            println!(
                "A {} service named '{}' is already queued in this session. Skipping.",
                side.key(),
                name
            );
            continue;
        }
        if let Err(e) = ensure_role_allows(&doc, side) {
            println!("{}", e);
            continue;
        }

        let mut entry = Table::new();
        match side {
            ServiceSide::Client => {
                let remote = match &existing_remote {
                    Some(r) => prompt(
                        "Server control channel address (host:port)",
                        Some(r),
                    )?,
                    None => prompt("Server control channel address (host:port)", Some("127.0.0.1:2333"))?,
                };
                validate_host_port(&remote, "server")?;
                entry.insert("remote_addr", value(remote.as_str()));
                let local = prompt("Local address to forward to (host:port)", Some("127.0.0.1:8080"))?;
                validate_host_port(&local, "local")?;
                entry.insert("local_addr", value(local.as_str()));
            }
            ServiceSide::Server => {
                let bind = prompt("Bind address of the service (host:port)", Some("0.0.0.0:8080"))?;
                validate_host_port(&bind, "bind")?;
                entry.insert("bind_addr", value(bind.as_str()));
            }
        }
        if prompt_select("Service type", &["tcp", "udp"])? == 1 {
            entry.insert("type", value("udp"));
        }
        let token = generate_token();
        entry.insert("token", value(token.as_str()));

        pending.push(PendingService {
            side,
            name: name.clone(),
            entry,
            generated_token: Some(token),
        });
        println!("Queued {} service '{}'.", side.key(), name);
    }

    if pending.is_empty() {
        println!("Nothing added.");
        return Ok(());
    }

    // Ensure sections exist for the queued sides.
    if pending.iter().any(|p| p.side == ServiceSide::Client) && doc.get("client").is_none() {
        let mut section = Table::new();
        let remote = pending
            .iter()
            .find_map(|p| {
                if p.side == ServiceSide::Client {
                    p.entry.get("remote_addr").and_then(Item::as_str).map(str::to_string)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| String::from("127.0.0.1:2333"));
        section.insert("remote_addr", value(remote.as_str()));
        section.insert("services", Item::Table(Table::new()));
        doc["client"] = Item::Table(section);
    }
    if pending.iter().any(|p| p.side == ServiceSide::Server) && doc.get("server").is_none() {
        let mut section = Table::new();
        section.insert("bind_addr", value(DEFAULT_SERVER_BIND_ADDR));
        section.insert("services", Item::Table(Table::new()));
        doc["server"] = Item::Table(section);
    }

    for p in pending {
        let r = insert_pending(&mut doc, p)?;
        // Same masking policy as batch mode: never echo the token itself.
        let token_note = if r["service"]["token"].is_string() {
            " (token ••••)"
        } else {
            ""
        };
        println!(
            "Added {} service '{}'{}",
            r["side"].as_str().unwrap_or_default(),
            r["name"].as_str().unwrap_or_default(),
            token_note
        );
    }

    save_with_skeleton(path, &doc, created)?;
    println!("Tokens were generated and written to the config file.");
    println!("Configuration saved to {}", path.display());
    Ok(())
}

/// Turn a parsed SPEC into a ready-to-insert service entry.
///
/// The spec's `server:` address is NOT written into the entry: it only
/// feeds the [client] section's remote_addr (one config, one server).
fn spec_to_pending(spec: ServiceSpec, side: ServiceSide) -> Result<PendingService> {
    let mut entry = Table::new();
    if spec.udp {
        entry.insert("type", value("udp"));
    }
    if let Some(l) = &spec.local_addr {
        entry.insert("local_addr", value(l.as_str()));
    }
    if let Some(b) = &spec.bind_addr {
        entry.insert("bind_addr", value(b.as_str()));
    }
    let generated = if spec.token.is_none() {
        Some(generate_token())
    } else {
        None
    };
    let token = spec.token.clone().or_else(|| generated.clone()).unwrap();
    entry.insert("token", value(token.as_str()));
    Ok(PendingService {
        side,
        name: spec.name,
        entry,
        generated_token: generated,
    })
}

/// Insert a pending service into the document, returning its JSON summary.
fn insert_pending(doc: &mut DocumentMut, p: PendingService) -> Result<Value> {
    let side = p.side;
    let name = p.name;

    let generated_token = p.generated_token.clone();
    let entry = p.entry;

    let section = doc[side.key()]
        .as_table_like_mut()
        .ok_or_else(|| anyhow!("[{}] is not a table", side.key()))?;
    let services = section
        .get_mut("services")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| anyhow!("[{}.services] is not a table", side.key()))?;
    services.insert(&name, Item::Table(entry));

    let service_json = item_to_json(&services.get(&name).cloned().unwrap());
    Ok(json!({
        "name": name,
        "side": side.key(),
        "service": service_json,
        "generated": match generated_token {
            Some(t) => json!({"token": t}),
            None => json!({}),
        },
    }))
}

/// Save the doc, prepending the skeleton when the file was just created.
///
/// The result is validated with the runtime rules before it touches the
/// disk: every `config add` path funnels through here, and a config the
/// watcher would silently reject must never be reported as added.
fn save_with_skeleton(path: &Path, doc: &DocumentMut, created: bool) -> Result<()> {
    crate::config::Config::validate(&doc.to_string())?;
    if created {
        write_atomic(path, &format!("{}\n{}", CONFIG_SKELETON, doc))
            .with_context(|| format!("Failed to write {}", path.display()))?;
    } else {
        save_document(path, doc)?;
    }
    Ok(())
}

fn build_generated(generated_token: &Option<String>, noise_plan: &NoisePlan) -> Value {
    let mut map = Map::new();
    if let Some(t) = generated_token {
        map.insert("token".to_string(), json!(t));
    }
    if let NoisePlan::Server {
        private_key,
        public_key,
    } = noise_plan
    {
        map.insert("noise_private_key".to_string(), json!(private_key));
        map.insert("noise_public_key".to_string(), json!(public_key));
    }
    Value::Object(map)
}

#[allow(clippy::too_many_arguments)]
fn print_add_human(
    path: &Path,
    side: ServiceSide,
    name: &str,
    service_json: &Value,
    generated_token: &Option<String>,
    noise_plan: &NoisePlan,
    created: bool,
) {
    if created {
        println!("Created a new configuration file: {}", path.display());
    }
    println!("Added {} service '{}' to {}:", side.key(), name, path.display());
    println!();
    println!("[{}.services.{}]", side.key(), name);
    if let Value::Object(map) = service_json {
        for (k, v) in map {
            println!("{} = {}", k, Value::to_string(v));
        }
    }
    println!();
    if let Some(t) = generated_token {
        println!("Generated token: {}", t);
        println!("(The same token must be configured on the other side.)");
    }
    if let NoisePlan::Server { public_key, .. } = noise_plan {
        println!("Generated noise keypair for [server.transport.noise]:");
        println!("  private key: stored in the configuration file");
        println!("  public key:  {}", public_key);
        println!("  (Give the public key to clients via `add --noise-key`.)");
    }
}

fn apply_noise_transport(doc: &mut DocumentMut, path: &Path, plan: &NoisePlan) -> Result<()> {
    let (section_key, key) = match plan {
        NoisePlan::None => return Ok(()),
        NoisePlan::Server { private_key, .. } => ("server", ("local_private_key", private_key)),
        NoisePlan::Client { remote_public_key } => {
            ("client", ("remote_public_key", remote_public_key))
        }
    };

    let section = doc[section_key]
        .as_table_like_mut()
        .ok_or_else(|| anyhow!("[{}] is not a table in {}", section_key, path.display()))?;

    if let Some(t) = section.get("transport").and_then(Item::as_table_like) {
        let existing = t.get("type").and_then(Item::as_str).unwrap_or("tcp");
        if existing != "tcp" && existing != "noise" {
            bail!(
                "[{}.transport] is already configured as '{}'; edit {} manually to switch transports",
                section_key,
                existing,
                path.display()
            );
        }
    }

    if section.get("transport").is_none() {
        section.insert("transport", Item::Table(Table::new()));
    }
    let transport = section
        .get_mut("transport")
        .and_then(Item::as_table_like_mut)
        .unwrap();
    transport.insert("type", value("noise"));

    if transport.get("noise").is_none() {
        transport.insert("noise", Item::Table(Table::new()));
    }
    let noise = transport
        .get_mut("noise")
        .and_then(Item::as_table_like_mut)
        .unwrap();
    noise.insert(key.0, value(key.1.as_str()));

    Ok(())
}

fn prompt(message: &str, default: Option<&str>) -> Result<String> {
    let mut input = dialoguer::Input::<String>::new().with_prompt(message.to_string());
    if let Some(d) = default {
        input = input.default(d.to_string());
    }
    Ok(input.interact_text()?)
}

/// Single-choice prompt backed by `dialoguer::Select`; returns the selected
/// index (default: the first entry).
fn prompt_select(message: &str, items: &[&str]) -> Result<usize> {
    Ok(dialoguer::Select::new()
        .with_prompt(message.to_string())
        .items(items)
        .default(0)
        .interact()?)
}

fn generate_token() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn validate_host_port(addr: &str, flag: &str) -> Result<()> {
    if addr.parse::<SocketAddr>().is_ok() {
        return Ok(());
    }
    // Non-IP hostnames are allowed; they only need a numeric port after the
    // last colon.
    match addr.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.parse::<u16>().is_ok() => Ok(()),
        _ => bail!("Invalid {} '{}': expected host:port", flag, addr),
    }
}

fn service_exists(doc: &DocumentMut, side: ServiceSide, name: &str) -> bool {
    doc.get(side.key())
        .and_then(|s| s.get("services"))
        .and_then(Item::as_table_like)
        .map(|services| services.get(name).is_some())
        .unwrap_or(false)
}

/// Whether a config key holds a secret; masked in human `config list` output.
fn is_sensitive_key(key: &str) -> bool {
    key.contains("token") || key.contains("private_key") || key.contains("password")
}

fn load_document(path: &Path) -> Result<DocumentMut> {
    match std::fs::read_to_string(path) {
        Ok(s) => s
            .parse::<DocumentMut>()
            .with_context(|| format!("Failed to parse {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(e) => Err(e).with_context(|| format!("Failed to read {}", path.display())),
    }
}

fn save_document(path: &Path, doc: &DocumentMut) -> Result<()> {
    write_atomic(path, &doc.to_string())
        .with_context(|| format!("Failed to write {}", path.display()))
}

fn item_to_json(item: &Item) -> Value {
    match item {
        Item::None => Value::Null,
        Item::Value(v) => value_to_json(v),
        Item::Table(t) => {
            let mut map = Map::new();
            for (k, v) in t.iter() {
                map.insert(k.to_string(), item_to_json(v));
            }
            Value::Object(map)
        }
        Item::ArrayOfTables(a) => Value::Array(
            a.iter()
                .map(|t| {
                    let mut map = Map::new();
                    for (k, v) in t.iter() {
                        map.insert(k.to_string(), item_to_json(v));
                    }
                    Value::Object(map)
                })
                .collect(),
        ),
    }
}

fn value_to_json(v: &toml_edit::Value) -> Value {
    match v {
        toml_edit::Value::String(s) => Value::String(s.value().clone()),
        toml_edit::Value::Integer(i) => json!(i.value()),
        toml_edit::Value::Float(f) => serde_json::Number::from_f64(*f.value())
            .map(Value::Number)
            .unwrap_or(Value::Null),
        toml_edit::Value::Boolean(b) => json!(b.value()),
        toml_edit::Value::Datetime(d) => Value::String(d.value().to_string()),
        toml_edit::Value::Array(a) => Value::Array(a.iter().map(value_to_json).collect()),
        toml_edit::Value::InlineTable(t) => {
            let mut map = Map::new();
            for (k, v) in t.iter() {
                map.insert(k.to_string(), value_to_json(v));
            }
            Value::Object(map)
        }
    }
}

const CONFIG_SKELETON: &str = r#"# rathole-x configuration file
#
# rathole-x runs as a client, a server, or both at once, depending on which
# of the [client] and [server] sections below are defined.
#
# Add a service with:
#   rathole-x config add --client "server:<SERVER:PORT>;name:my-service;local:<HOST:PORT>"
#   rathole-x config add --server "name:my-service;bind:<HOST:PORT>"
#
# ---------------------------------------------------------------------------
# Client side: connect to a rathole server and expose local services through it
# ---------------------------------------------------------------------------
#
# [client]
# remote_addr = "example.com:2333"   # control channel address of the server
# # default_token = "secret"         # default token for every client service
# # heartbeat_timeout = 40           # seconds before the control channel is declared dead
# # retry_interval = 1               # seconds between reconnection attempts
#
# # Optional transport configuration (default is plain TCP):
# # [client.transport]
# # type = "noise"                   # tcp | tls | noise | websocket
# # [client.transport.noise]
# # remote_public_key = "..."        # the server's noise public key
#
# [client.services.my-service]
# local_addr = "127.0.0.1:80"        # local address the traffic is forwarded to
# # token = "secret"                 # overrides default_token
# # type = "tcp"                     # tcp (default) or udp
#
# ---------------------------------------------------------------------------
# Server side: accept clients and publish their services
# ---------------------------------------------------------------------------
#
# [server]
# bind_addr = "0.0.0.0:2333"         # control channel bind address
# # default_token = "secret"         # default token for every server service
# # heartbeat_interval = 30          # seconds between heartbeats
#
# # Optional transport configuration (default is plain TCP):
# # [server.transport]
# # type = "noise"                   # tcp | tls | noise | websocket
# # [server.transport.noise]
# # local_private_key = "..."        # generate with: rathole-x genkey
#
# [server.services.my-service]
# bind_addr = "0.0.0.0:80"           # address the service is published on
# # token = "secret"                 # overrides default_token
# # type = "tcp"                     # tcp (default) or udp
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_configs_parse() {
        let server: crate::config::Config =
            toml::from_str(DEFAULT_SERVER_CONFIG).expect("server config must parse");
        assert!(server.server.is_some(), "server config defines [server]");
        assert!(server.client.is_none(), "server config has no [client]");
        assert!(server.server.unwrap().services.is_empty());

        let client: crate::config::Config =
            toml::from_str(DEFAULT_CLIENT_CONFIG).expect("client config must parse");
        assert!(client.client.is_some(), "client config defines [client]");
        assert!(client.server.is_none(), "client config has no [server]");
        assert!(client.client.unwrap().services.is_empty());
    }

    #[test]
    fn ensure_role_config_creates_then_validates() {
        let dir = std::env::temp_dir().join("rathole-x-install-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let _ = std::fs::remove_file(&path);

        assert!(ensure_role_config(&path, ServiceRole::Server).expect("creates config"));
        assert!(!ensure_role_config(&path, ServiceRole::Server).expect("idempotent on valid config"));

        // Corrupt the config: must now fail validation instead of overwriting
        std::fs::write(&path, "not [valid toml").unwrap();
        assert!(ensure_role_config(&path, ServiceRole::Server).is_err());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    // Windows-oriented permission probe: set_readonly(false) is the point
    // of the test; the Unix world-writable side effect is irrelevant here.
    #[allow(clippy::permissions_set_readonly_false)]
    fn writability_probe_reflects_permissions() {
        let dir = std::env::temp_dir().join("rathole-x-writable-test");
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.toml");
        // Clear a read-only leftover from a previous failed run
        #[cfg(windows)]
        {
            if let Ok(md) = std::fs::metadata(&config) {
                let mut perms = md.permissions();
                perms.set_readonly(false);
                let _ = std::fs::set_permissions(&config, perms);
            }
        }
        std::fs::write(&config, "[server]\nbind_addr = \"0.0.0.0:1\"\n[server.services]\n").unwrap();

        // Existing writable file: probe succeeds
        assert!(writable_by_current_user(&config));

        // Read-only attribute blocks writes on Windows (best-effort check)
        #[cfg(windows)]
        {
            let md = std::fs::metadata(&config).unwrap();
            let mut perms = md.permissions();
            perms.set_readonly(true);
            std::fs::set_permissions(&config, perms).unwrap();
            assert!(!writable_by_current_user(&config), "read-only file not writable");
            // Restore writability so the cleanup below (and the next test
            // run) can delete the file
            let mut perms = std::fs::metadata(&config).unwrap().permissions();
            perms.set_readonly(false);
            std::fs::set_permissions(&config, perms).unwrap();
        }

        // Missing file: directory probe
        let missing = dir.join("missing.toml");
        assert!(writable_by_current_user(&missing));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn version_compat_gate() {
        let dir = std::env::temp_dir().join("rathole-x-ver-test");
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.toml");
        let auth = dir.join("version.toml");
        let _ = std::fs::remove_file(&auth);

        // No policy file: no restriction
        check_version_compat(&config).unwrap();

        // No version stamp (legacy policy): no restriction
        std::fs::write(&auth, "# legacy stamp file\n").unwrap();
        check_version_compat(&config).unwrap();

        // Matching major version: allowed
        let major = crate::cli::major_version();
        std::fs::write(&auth, format!("version = {}\n", major)).unwrap();
        check_version_compat(&config).unwrap();

        // Mismatched major version: refused
        std::fs::write(&auth, format!("version = {}\n", major + 1)).unwrap();
        assert!(check_version_compat(&config).is_err());

        // Corrupt policy: no restriction (install owns the file)
        std::fs::write(&auth, "not [valid").unwrap();
        check_version_compat(&config).unwrap();

        std::fs::remove_file(&auth).ok();
    }

    #[test]
    fn parse_spec_accepts_and_rejects() {
        // Valid client spec
        let s = parse_spec("server:srv.com:2333;name:nas;local:127.0.0.1:22;type:tcp", ServiceSide::Client).unwrap();
        assert_eq!(s.name, "nas");
        assert_eq!(s.remote_addr.as_deref(), Some("srv.com:2333"));
        assert_eq!(s.local_addr.as_deref(), Some("127.0.0.1:22"));
        assert!(!s.udp);

        // Valid server spec, udp, auto token
        let s = parse_spec("name:dns;bind:0.0.0.0:53;type:udp", ServiceSide::Server).unwrap();
        assert!(s.udp);
        assert_eq!(s.bind_addr.as_deref(), Some("0.0.0.0:53"));
        assert!(s.token.is_none());

        // Missing name
        assert!(parse_spec("local:1.2.3.4:80", ServiceSide::Client).is_err());
        // Unknown key
        assert!(parse_spec("name:x;nope:y", ServiceSide::Client).is_err());
        // Client key on server side
        assert!(parse_spec("name:x;local:1.2.3.4:80", ServiceSide::Server).is_err());
        // Bad addr
        assert!(parse_spec("name:x;local:not-a-port", ServiceSide::Client).is_err());
        // Server spec without bind
        assert!(parse_spec("name:x", ServiceSide::Server).is_err());
    }

    #[test]
    fn per_service_remote_addr_is_rejected() {
        // Upstream alignment: one client config connects to one server, so
        // a per-service remote_addr is an unknown field, not an override
        let toml = r#"
[client]
remote_addr = "srv-a.com:2333"

[client.services]

[client.services.multi]
local_addr = "127.0.0.1:22"
remote_addr = "srv-b.com:2333"
token = "t"
"#;
        assert!(toml::from_str::<crate::config::Config>(toml).is_err());
    }

    #[test]
    fn add_single_rejects_mismatched_remote() {
        let dir = std::env::temp_dir().join("rathole-x-add-remote-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[client]\nremote_addr = \"srv-a.com:2333\"\n\
             [client.services.foo]\nlocal_addr = \"127.0.0.1:22\"\ntoken = \"t\"\n",
        )
        .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let args = AddArgs {
            entry: Some("bar".to_string()),
            remote_addr: Some("srv-b.com:2333".to_string()),
            local_addr: Some("127.0.0.1:80".to_string()),
            yes: true,
            ..Default::default()
        };
        assert!(
            run_add_single(&args, &path).is_err(),
            "a different server must be rejected"
        );
        assert_eq!(
            before,
            std::fs::read_to_string(&path).unwrap(),
            "a rejected add must leave the file untouched"
        );

        // Same server as the section default stays a no-op flag
        let args = AddArgs {
            remote_addr: Some("srv-a.com:2333".to_string()),
            ..args
        };
        assert!(run_add_single(&args, &path).is_ok());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_batch_rejects_conflicting_servers() {
        let dir = std::env::temp_dir().join("rathole-x-batch-remote-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        let args = AddArgs {
            client_specs: vec![
                "name:a;server:srv-a.com:2333;local:127.0.0.1:22".to_string(),
                "name:b;server:srv-b.com:2333;local:127.0.0.1:80".to_string(),
            ],
            yes: true,
            ..Default::default()
        };
        assert!(
            run_add_batch(&args, &path).is_err(),
            "conflicting servers in one batch must fail"
        );
        assert!(!path.exists(), "a failed batch must not create the file");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_single_duplicate_is_side_effect_free() {
        let dir = std::env::temp_dir().join("rathole-x-add-dup-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[server]\nbind_addr = \"0.0.0.0:2333\"\n\
             [server.transport]\ntype = \"noise\"\n\
             [server.transport.noise]\nlocal_private_key = \"ORIGINAL\"\n\
             [server.services.mysvc]\nbind_addr = \"0.0.0.0:8080\"\ntoken = \"t\"\n",
        )
        .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        // Re-adding the same service with --noise would rotate the private
        // key if the duplicate check ran after the write
        let args = AddArgs {
            entry: Some("mysvc".to_string()),
            bind_addr: Some("0.0.0.0:9090".to_string()),
            noise: true,
            yes: true,
            ..Default::default()
        };
        assert!(
            run_add_single(&args, &path).is_err(),
            "duplicate name must fail"
        );
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after, "a failed add must leave the file untouched");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_batch_rejects_internal_duplicates() {
        let dir = std::env::temp_dir().join("rathole-x-batch-dup-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let _ = std::fs::remove_file(&path);

        let args = AddArgs {
            client_specs: vec![
                "name:dup;server:srv.com:2333;local:127.0.0.1:8001".to_string(),
                "name:dup;server:srv.com:2333;local:127.0.0.1:8002".to_string(),
            ],
            yes: true,
            ..Default::default()
        };
        assert!(
            run_add_batch(&args, &path).is_err(),
            "duplicate names in one batch must fail"
        );
        assert!(!path.exists(), "a failed batch must not create the file");

        std::fs::remove_dir_all(&dir).ok();
    }
}
