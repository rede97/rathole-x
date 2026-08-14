use std::path::PathBuf;

use clap::{AppSettings, ArgEnum, Args, Parser, Subcommand};
use lazy_static::lazy_static;

#[derive(ArgEnum, Clone, Debug, Copy)]
pub enum KeypairType {
    X25519,
    X448,
}

lazy_static! {
    static ref VERSION: &'static str = env!("VERGEN_BUILD_SEMVER");
    static ref LONG_VERSION: String = format!(
        "
Build Timestamp:     {}
Build Version:       {}
cargo Target Triple: {}
cargo Profile:       {}
cargo Features:      {}
",
        env!("VERGEN_BUILD_TIMESTAMP"),
        env!("VERGEN_BUILD_SEMVER"),
        env!("VERGEN_CARGO_TARGET_TRIPLE"),
        env!("VERGEN_CARGO_PROFILE"),
        env!("VERGEN_CARGO_FEATURES")
    );
}

/// The major version of this build, parsed from the semver banner.
/// Used as the config-compatibility stamp written into version.toml.
pub fn major_version() -> u32 {
    VERSION
        .split('.')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

#[derive(Parser, Debug, Default, Clone)]
#[clap(
    name = "rathole-x",
    about,
    version(*VERSION),

    long_version(LONG_VERSION.as_str()),
    after_help = "GitHub: https://github.com/rede97/rathole-x",
    setting(AppSettings::DeriveDisplayOrder),
)]
pub struct Cli {
    #[clap(subcommand)]
    pub command: Option<Commands>,

    /// Redirect all output to this file (used by the UAC elevation relay)
    #[clap(long, hide = true, global = true)]
    pub elevated_log: Option<PathBuf>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Commands {
    /// Run with a configuration file
    ///
    /// When `--config` is omitted, the OS default path is used
    /// (%ProgramData%\rathole-x\rathole-x.toml on Windows,
    /// /etc/rathole-x.toml on Linux)
    Run(RunArgs),

    /// Generate a keypair for the use of the noise protocol
    Genkey {
        /// The DH function to use
        #[clap(long, arg_enum, value_name = "CURVE")]
        curve: Option<KeypairType>,
    },

    /// Manage the service configuration (services and global fields)
    Config {
        #[clap(subcommand)]
        cmd: ConfigCmd,
    },

    /// Show the service state and the configuration as a tree
    Status(StatusArgs),

    /// Install rathole-x as a system service
    Install(InstallArgs),

    /// Uninstall the rathole-x system service
    Uninstall(UninstallArgs),
    /// Start, stop or restart an installed service
    Service {
        #[clap(subcommand)]
        cmd: ServiceCmd,
    },

    /// Update the installed binary in place (stop, replace, start)
    Upgrade(UpgradeArgs),

    /// Entry point used by the Windows service manager. Not for interactive use
    ServiceRun {
        /// The configuration file the service runs with
        #[clap(long, parse(from_os_str))]
        config: PathBuf,
    },
}

/// Subcommands of `rathole-x config`: service and global field management.
#[derive(Subcommand, Debug, Clone)]
pub enum ConfigCmd {
    /// Add a service to the configuration file
    Add(AddArgs),

    /// Remove a service from the configuration file
    Remove(RemoveArgs),

    /// List the services in the configuration file
    List(ListArgs),

    /// Update global fields of the [client] or [server] section
    Set(SetArgs),
}


#[derive(Args, Debug, Clone, Default)]
pub struct ListArgs {
    /// The path to the configuration file
    #[clap(parse(from_os_str), short, long)]
    pub config: Option<PathBuf>,

    /// Short name of the installed service the operation targets. When
    /// omitted and exactly one service is installed, that one is used.
    #[clap(long)]
    pub name: Option<String>,

    /// Print the result as a single JSON object
    #[clap(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone, Default)]
pub struct RunArgs {
    #[clap(parse(from_os_str), short, long)]
    pub config: Option<PathBuf>,

    /// Run as a server
    #[clap(long, short, group = "mode")]
    pub server: bool,

    /// Run as a client
    #[clap(long, group = "mode")]
    pub client: bool,
}

#[derive(ArgEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ServiceTypeArg {
    #[default]
    Tcp,
    Udp,
}

/// The role a system service runs as: exactly one of server or client.
#[derive(ArgEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoleArg {
    Server,
    Client,
}

impl RoleArg {
    pub fn key(&self) -> &'static str {
        match self {
            RoleArg::Server => "server",
            RoleArg::Client => "client",
        }
    }
}

#[derive(Args, Debug, Clone, Default)]
pub struct AddArgs {
    /// Name of the service entry to add (optional: interactive or
    /// --client/--server spec mode adds one or more entries without it)
    pub entry: Option<String>,

    /// Named server profiles, repeatable: "name:<id>;server:<host:port>".
    ///
    /// Referenced from --client specs via "remote:<id>" instead of an
    /// inline "server:" address. Pure CLI sugar: resolved to concrete
    /// addresses at write time, the config file stays upstream-compatible.
    #[clap(long = "remote", value_name = "SPEC", multiple_occurrences(true))]
    pub remote_specs: Vec<String>,

    /// Client service entries in compact form, repeatable.
    ///
    /// SPEC = "key:value;key:value;..." with keys:
    ///   name (required), server (control channel host:port) or remote
    ///   (named profile from --remote), local
    ///   (local forward target, required), token (auto-generated when
    ///   omitted), type (tcp|udp, default tcp).
    /// The server key also acts as the [client] remote_addr default when the
    /// section is created, and as a per-service override otherwise (a client
    /// may connect to several servers).
    #[clap(long = "client", value_name = "SPEC", multiple_occurrences(true))]
    pub client_specs: Vec<String>,

    /// Server service entries in compact form, repeatable.
    ///
    /// SPEC keys: name (required), bind (published address, required),
    /// token (auto-generated when omitted), type (tcp|udp, default tcp).
    #[clap(long = "server", value_name = "SPEC", multiple_occurrences(true))]
    pub server_specs: Vec<String>,

    /// Client control channel address (host:port)
    ///
    /// Also sets [client] remote_addr when the section is created
    #[clap(long)]
    pub remote_addr: Option<String>,

    /// Server-side service bind address (host:port)
    #[clap(long)]
    pub bind_addr: Option<String>,

    /// Client-side local forward target (host:port)
    #[clap(long)]
    pub local_addr: Option<String>,

    /// Auth token for the service (default: auto-generated)
    #[clap(long)]
    pub token: Option<String>,

    /// Use the noise transport for the control channel
    ///
    /// Server side: generates an X25519 keypair and stores the private key.
    /// Client side: requires --noise-key with the server public key
    #[clap(long)]
    pub noise: bool,

    /// Server public key for the noise transport (client side only)
    #[clap(long)]
    pub noise_key: Option<String>,

    /// The transport type of the service
    #[clap(long = "type", arg_enum, default_value = "tcp")]
    pub service_type: ServiceTypeArg,

    /// The path to the configuration file
    #[clap(parse(from_os_str), short, long)]
    pub config: Option<PathBuf>,

    /// Short name of the installed service the operation targets. When
    /// omitted and exactly one service is installed, that one is used.
    #[clap(long)]
    pub name: Option<String>,

    /// Print the result as a single JSON object
    #[clap(long)]
    pub json: bool,

    /// Never prompt; fail when required parameters are missing
    #[clap(long)]
    pub yes: bool,
}
#[derive(Args, Debug, Clone, Default)]
pub struct RemoveArgs {
    /// Name of the service entry to remove
    pub entry: String,

    /// The path to the configuration file
    #[clap(parse(from_os_str), short, long)]
    pub config: Option<PathBuf>,

    /// Short name of the installed service the operation targets. When
    /// omitted and exactly one service is installed, that one is used.
    #[clap(long)]
    pub name: Option<String>,

    /// Print the result as a single JSON object
    #[clap(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone, Default)]
pub struct InstallArgs {
    /// The role of the service: server or client (exactly one)
    #[clap(arg_enum, value_name = "ROLE")]
    pub role: Option<RoleArg>,

    /// The path to the configuration file the service runs with
    ///
    /// Defaults to the OS default directory
    /// (%ProgramData%\rathole-x\<name>.toml on Windows,
    /// /etc/rathole-x/<name>.toml on Linux)
    #[clap(parse(from_os_str), short, long)]
    pub config: Option<PathBuf>,

    /// Short name of the service (SCM name: rathole-x-<role>-<name>).
    /// Defaults to "default".
    #[clap(long)]
    pub name: Option<String>,

    /// Allow non-admin users to modify the service configuration
    ///
    /// Grants modify rights on the config so the CLI can update it without
    /// elevation (the service hot-reloads the change). Without this flag the
    /// config stays admin-only and the CLI elevates (UAC) before writing.
    /// No policy file is stored: the CLI probes the actual file permissions.
    #[clap(long)]
    pub allow_user_config: bool,

    /// Confirm and execute
    ///
    /// Without this flag only the usage of `install` is shown.
    #[clap(long)]
    pub yes: bool,
}

#[derive(Args, Debug, Clone, Default)]
pub struct StatusArgs {
    /// The path to the configuration file
    ///
    /// Defaults to the OS default path
    /// (%ProgramData%\rathole-x\rathole-x.toml on Windows,
    /// /etc/rathole-x.toml on Linux)
    #[clap(parse(from_os_str), short, long)]
    pub config: Option<PathBuf>,

    /// Short name of the service to inspect. When omitted, every installed
    /// service is listed.
    #[clap(long)]
    pub name: Option<String>,

    /// Print the result as a single JSON object
    #[clap(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone, Default)]
pub struct UninstallArgs {
    /// Short name of the service to remove. When omitted and exactly one
    /// service is installed, that one is used.
    #[clap(long)]
    pub name: Option<String>,

    /// The path to the configuration file the service runs with
    ///
    /// Used for cleanup: version.toml is removed when the last service goes,
    /// the config file only with --purge.
    #[clap(parse(from_os_str), short, long)]
    pub config: Option<PathBuf>,

    /// Also delete the configuration file
    ///
    /// Without --purge the network config is kept; version.toml is removed
    /// with the last service either way.
    #[clap(long)]
    pub purge: bool,

    /// Remove EVERY installed service, their configs and the shared binary
    #[clap(long)]
    pub all: bool,

    /// Confirm and execute
    ///
    /// Without this flag only the usage of `uninstall` is shown.
    #[clap(long)]
    pub yes: bool,
}

#[derive(ArgEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportTypeArg {
    Tcp,
    Tls,
    Noise,
    Websocket,
}

impl TransportTypeArg {
    pub fn key(&self) -> &'static str {
        match self {
            TransportTypeArg::Tcp => "tcp",
            TransportTypeArg::Tls => "tls",
            TransportTypeArg::Noise => "noise",
            TransportTypeArg::Websocket => "websocket",
        }
    }
}

#[derive(Args, Debug, Clone, Default)]
pub struct SetArgs {
    /// Update the [client] section
    #[clap(long, group = "side")]
    pub client: bool,

    /// Update the [server] section
    #[clap(long, group = "side")]
    pub server: bool,

    /// [client] remote_addr: the server control channel address (host:port)
    #[clap(long)]
    pub remote_addr: Option<String>,

    /// [server] bind_addr: the control channel bind address (host:port)
    #[clap(long)]
    pub bind_addr: Option<String>,

    /// default_token applied to services without their own token
    #[clap(long)]
    pub default_token: Option<String>,

    /// [client] prefer IPv6 when resolving remote_addr
    #[clap(long)]
    pub prefer_ipv6: Option<bool>,

    /// [client] control channel heartbeat timeout in seconds
    #[clap(long)]
    pub heartbeat_timeout: Option<u64>,

    /// [client] seconds between control channel reconnection attempts
    #[clap(long)]
    pub retry_interval: Option<u64>,

    /// [server] seconds between control channel heartbeats
    #[clap(long)]
    pub heartbeat_interval: Option<u64>,

    /// Control channel transport type
    #[clap(long, arg_enum)]
    pub transport: Option<TransportTypeArg>,

    /// [transport.noise] generate a fresh server noise keypair (server only)
    #[clap(long)]
    pub noise: bool,

    /// [transport.noise] the server's noise public key (client only)
    #[clap(long)]
    pub noise_key: Option<String>,

    /// [transport.tls] trusted_root certificate path (client only)
    #[clap(long)]
    pub trusted_root: Option<String>,

    /// [transport.tls] hostname expected in the server certificate (client only)
    #[clap(long)]
    pub hostname: Option<String>,

    /// [transport.tls] pkcs12 identity bundle path (server only)
    #[clap(long)]
    pub pkcs12: Option<String>,

    /// [transport.tls] pkcs12 password (server only)
    #[clap(long)]
    pub pkcs12_password: Option<String>,

    /// [transport.websocket] use TLS inside the websocket tunnel
    #[clap(long)]
    pub ws_tls: Option<bool>,

    /// [transport.tcp] enable TCP_NODELAY
    #[clap(long)]
    pub nodelay: Option<bool>,

    /// [transport.tcp] keepalive seconds
    #[clap(long)]
    pub keepalive_secs: Option<u64>,

    /// [transport.tcp] keepalive interval
    #[clap(long)]
    pub keepalive_interval: Option<u64>,

    /// [transport.tcp] HTTP proxy URL for the control channel
    #[clap(long)]
    pub proxy: Option<String>,

    /// The path to the configuration file
    #[clap(parse(from_os_str), short, long)]
    pub config: Option<PathBuf>,

    /// Short name of the installed service the operation targets. When
    /// omitted and exactly one service is installed, that one is used.
    #[clap(long)]
    pub name: Option<String>,

    /// Print the result as a single JSON object
    #[clap(long)]
    pub json: bool,
}

/// Actions of `rathole-x service`.
#[derive(Subcommand, Debug, Clone)]
pub enum ServiceCmd {
    /// Start an installed service
    Start(ServiceArgs),
    /// Stop an installed service
    Stop(ServiceArgs),
    /// Restart an installed service (stop, then start)
    Restart(ServiceArgs),
}

#[derive(Args, Debug, Clone, Default)]
pub struct ServiceArgs {
    /// Short name of the service. When omitted and exactly one service is
    /// installed, that one is used.
    #[clap(long)]
    pub name: Option<String>,

    /// Apply to every installed service
    #[clap(long)]
    pub all: bool,
}

#[derive(Args, Debug, Clone, Default)]
pub struct UpgradeArgs {
    /// Confirm and execute
    ///
    /// Without this flag only the usage of `upgrade` is shown.
    #[clap(long)]
    pub yes: bool,
}
