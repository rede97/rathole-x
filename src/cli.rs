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
    global_setting(AppSettings::DeriveDisplayOrder),
)]
pub struct Cli {
    #[clap(subcommand)]
    pub command: Option<Commands>,


    /// Redirect all output to this file (used by the UAC elevation relay)
    #[clap(long, hide = true, global = true)]
    pub elevated_log: Option<PathBuf>,

    /// Mark this process as already confirmed (used by the UAC elevation
    /// relay: the parent confirms interactively, the hidden TTY-less child
    /// re-runs the same command line and must not ask again)
    #[clap(long, hide = true, global = true)]
    pub confirmed: bool,
}

impl Cli {
    /// Whether this invocation is the hidden SCM service entry point
    /// (`service run`). That path installs its own file subscriber in
    /// `service_main_inner`, so main must not pre-install the stdout
    /// subscriber (a global default makes the later try_init a silent no-op
    /// and every service log line would be lost).
    pub fn is_service_run(&self) -> bool {
        matches!(
            self.command,
            Some(Commands::Service {
                cmd: ServiceCmd::Run { .. }
            })
        )
    }
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

    /// Install, uninstall, start, stop or restart an installed service
    Service {
        #[clap(subcommand)]
        cmd: ServiceCmd,
    },

    /// Update the installed binary in place (stop, replace, start)
    Upgrade(UpgradeArgs),
}

/// Subcommands of `rathole-x config`: service and global field management.
#[derive(Subcommand, Debug, Clone)]
pub enum ConfigCmd {
    /// Add a service to the configuration file
    Add(Box<AddArgs>),

    /// Remove a service from the configuration file
    Remove(RemoveArgs),

    /// List the services in the configuration file
    List(ListArgs),

    /// Update global fields of the [client] or [server] section
    Set(Box<SetArgs>),

    /// Import supported fields and services from an old config file
    Import(ImportArgs),
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
    #[clap(conflicts_with_all = &["client-specs", "server-specs"])]
    pub entry: Option<String>,

    /// Client service entries in compact form, repeatable.
    ///
    /// SPEC = "key:value;key:value;..." with keys:
    ///   name (required), local (local forward target, required),
    ///   token (auto-generated when omitted), type (tcp|udp, default tcp).
    /// The control channel server is NOT part of a SPEC: one client config
    /// connects to exactly one server, set once per config with
    /// `config set --client --remote-addr <host:port>`.
    #[clap(long = "client", value_name = "SPEC", multiple_occurrences(true))]
    pub client_specs: Vec<String>,

    /// Server service entries in compact form, repeatable.
    ///
    /// SPEC keys: name (required), bind (published address, required),
    /// token (auto-generated when omitted), type (tcp|udp, default tcp).
    #[clap(long = "server", value_name = "SPEC", multiple_occurrences(true))]
    pub server_specs: Vec<String>,

    /// Server-side service bind address (host:port)
    ///
    /// Mutually exclusive with --local-addr: the two select opposite sides
    #[clap(long, conflicts_with = "local-addr")]
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
    ///
    /// Requires --noise
    #[clap(long, requires = "noise")]
    pub noise_key: Option<String>,

    /// The transport type of the service
    #[clap(long = "type", arg_enum, default_value = "tcp")]
    pub service_type: ServiceTypeArg,

    /// The path to the configuration file
    #[clap(parse(from_os_str), short, long, conflicts_with = "name")]
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
pub struct ImportArgs {
    /// Path of the old configuration file to import supported fields and
    /// services from (upstream rathole or an older rathole-x config).
    /// Unknown keys are skipped and reported; existing service entries are
    /// never overwritten; the target config's role must match.
    #[clap(value_name = "OLD_CONFIG", parse(from_os_str))]
    pub source: PathBuf,

    /// The path to the configuration file (import target)
    #[clap(parse(from_os_str), short, long, conflicts_with = "name")]
    pub config: Option<PathBuf>,

    /// Short name of the installed service the operation targets. When
    /// omitted and exactly one service is installed, that one is used
    #[clap(long)]
    pub name: Option<String>,

    /// Print the result as a single JSON object
    #[clap(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone, Default)]
pub struct RemoveArgs {
    /// Name of the service entry to remove
    pub entry: String,

    /// The path to the configuration file
    #[clap(parse(from_os_str), short, long, conflicts_with = "name")]
    pub config: Option<PathBuf>,

    /// Short name of the installed service the operation targets. When
    /// omitted and exactly one service is installed, that one is used.
    #[clap(long)]
    pub name: Option<String>,

    /// Print the result as a single JSON object
    #[clap(long)]
    pub json: bool,

    /// Skip the confirmation prompt before removing the service
    #[clap(long)]
    pub yes: bool,
}

#[derive(Args, Debug, Clone, Default)]
pub struct InstallArgs {
    /// The role of the service: server or client (exactly one)
    #[clap(arg_enum, value_name = "ROLE", required = true)]
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

    /// Print the result as a single JSON object
    #[clap(long)]
    pub json: bool,

    /// Skip the confirmation prompt
    ///
    /// Without --yes an interactive terminal shows the install plan and asks
    /// for confirmation; an unattended invocation fails with an actionable error.
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
    #[clap(long, conflicts_with = "all")]
    pub name: Option<String>,

    /// The path to the configuration file the service runs with
    ///
    /// Used for cleanup: version.toml is removed when the last service goes,
    /// the config file only with --purge.
    #[clap(parse(from_os_str), short, long, conflicts_with = "all")]
    pub config: Option<PathBuf>,

    /// Also delete the configuration file
    ///
    /// With --all this makes the full removal explicit: every service and
    /// every config. Without --all/--purge the network config is kept;
    /// version.toml is removed with the last service either way.
    #[clap(long)]
    pub purge: bool,

    /// Remove EVERY installed service; with --purge also remove their configs
    #[clap(long, conflicts_with_all = &["name", "config"])]
    pub all: bool,

    /// Print the result as a single JSON object
    #[clap(long)]
    pub json: bool,

    /// Skip the confirmation prompt
    ///
    /// Without --yes an interactive terminal shows the removal plan and asks
    /// for confirmation; an unattended invocation fails with an actionable error.
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
    /// Update the [client] section (optional when the config already
    /// declares exactly one role — the side is then inferred)
    #[clap(long, group = "side")]
    pub client: bool,

    /// Update the [server] section (optional when the config already
    /// declares exactly one role — the side is then inferred)
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
    ///
    /// Accepts an optional value: `--prefer-ipv6` means `--prefer-ipv6 true`
    #[clap(long, min_values(0), max_values(1), default_missing_value("true"))]
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
    ///
    /// Accepts an optional value: `--ws-tls` means `--ws-tls true`
    #[clap(long, min_values(0), max_values(1), default_missing_value("true"))]
    pub ws_tls: Option<bool>,

    /// [transport.tcp] enable TCP_NODELAY
    ///
    /// Accepts an optional value: `--nodelay` means `--nodelay true`
    #[clap(long, min_values(0), max_values(1), default_missing_value("true"))]
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
    #[clap(parse(from_os_str), short, long, conflicts_with = "name")]
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
    /// Install a service (exactly one role: server or client)
    Install(InstallArgs),
    /// Uninstall a service (or every service with --all)
    Uninstall(UninstallArgs),
    /// Start an installed service
    Start(ServiceArgs),
    /// Stop an installed service
    Stop(ServiceArgs),
    /// Restart an installed service (stop, then start)
    Restart(ServiceArgs),

    /// Entry point used by the Windows service manager. Not for interactive use
    #[clap(hide = true)]
    Run {
        /// The configuration file the service runs with
        #[clap(long, parse(from_os_str))]
        config: PathBuf,
    },
}

#[derive(Args, Debug, Clone, Default)]
pub struct ServiceArgs {
    /// Short name of the service. When omitted and exactly one service is
    /// installed, that one is used.
    #[clap(long, conflicts_with = "all")]
    pub name: Option<String>,

    /// Apply to every installed service
    #[clap(long, conflicts_with = "name")]
    pub all: bool,

    /// Print the result as a single JSON object
    #[clap(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone, Default)]
pub struct UpgradeArgs {
    /// Print the result as a single JSON object
    #[clap(long)]
    pub json: bool,

    /// Skip the confirmation prompt
    ///
    /// Without --yes an interactive terminal asks for confirmation;
    /// an unattended invocation fails with an actionable error.
    #[clap(long)]
    pub yes: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> std::result::Result<Cli, clap::Error> {
        Cli::try_parse_from(argv)
    }

    fn set_args(cli: Cli) -> SetArgs {
        match cli.command {
            Some(Commands::Config {
                cmd: ConfigCmd::Set(s),
            }) => *s,
            other => panic!("expected `config set`, got {:?}", other),
        }
    }

    #[test]
    fn add_entry_conflicts_with_spec_flags() {
        assert!(parse(&[
            "rathole-x",
            "config",
            "add",
            "mysvc",
            "--client",
            "name:a;server:127.0.0.1:2333;local:127.0.0.1:8080",
        ])
        .is_err());
        assert!(parse(&[
            "rathole-x",
            "config",
            "add",
            "mysvc",
            "--server",
            "name:a;bind:0.0.0.0:8080",
        ])
        .is_err());
        // Each form alone still parses.
        assert!(parse(&["rathole-x", "config", "add", "mysvc", "--yes"]).is_ok());
        assert!(parse(&[
            "rathole-x",
            "config",
            "add",
            "--client",
            "name:a;server:127.0.0.1:2333;local:127.0.0.1:8080",
        ])
        .is_ok());
    }

    #[test]
    fn add_bind_addr_conflicts_with_local_addr() {
        assert!(parse(&[
            "rathole-x",
            "config",
            "add",
            "mysvc",
            "--yes",
            "--bind-addr",
            "0.0.0.0:8080",
            "--local-addr",
            "127.0.0.1:8080",
        ])
        .is_err());
        assert!(parse(&[
            "rathole-x",
            "config",
            "add",
            "mysvc",
            "--yes",
            "--bind-addr",
            "0.0.0.0:8080",
        ])
        .is_ok());
    }

    #[test]
    fn add_noise_key_requires_noise() {
        assert!(parse(&[
            "rathole-x",
            "config",
            "add",
            "mysvc",
            "--yes",
            "--noise-key",
            "abc"
        ])
        .is_err());
        assert!(parse(&[
            "rathole-x",
            "config",
            "add",
            "mysvc",
            "--yes",
            "--noise",
            "--noise-key",
            "abc",
        ])
        .is_ok());
    }

    #[test]
    fn run_mode_flags_conflict() {
        // The shared `group = "mode"` makes --client/--server mutually
        // exclusive in clap 3.2 (ArgGroup defaults to multiple(false)).
        assert!(parse(&["rathole-x", "run", "--client", "--server"]).is_err());
        assert!(parse(&["rathole-x", "run", "--client"]).is_ok());
    }

    #[test]
    fn set_bool_flags_accept_optional_value() {
        // Bare flag: the missing value defaults to "true".
        let s = set_args(parse(&["rathole-x", "config", "set", "--client", "--nodelay"]).unwrap());
        assert_eq!(s.nodelay, Some(true));
        let s =
            set_args(parse(&["rathole-x", "config", "set", "--client", "--prefer-ipv6"]).unwrap());
        assert_eq!(s.prefer_ipv6, Some(true));
        let s = set_args(parse(&["rathole-x", "config", "set", "--client", "--ws-tls"]).unwrap());
        assert_eq!(s.ws_tls, Some(true));

        // Explicit values keep working in both spellings.
        let s = set_args(
            parse(&[
                "rathole-x",
                "config",
                "set",
                "--client",
                "--nodelay",
                "false",
            ])
            .unwrap(),
        );
        assert_eq!(s.nodelay, Some(false));
        let s =
            set_args(parse(&["rathole-x", "config", "set", "--client", "--ws-tls=false"]).unwrap());
        assert_eq!(s.ws_tls, Some(false));

        // Unset stays None.
        let s = set_args(parse(&["rathole-x", "config", "set", "--client"]).unwrap());
        assert_eq!(s.nodelay, None);
    }

    #[test]
    fn privileged_commands_accept_json_flag() {
        assert!(parse(&["rathole-x", "service", "start", "--json"]).is_ok());
        assert!(parse(&["rathole-x", "service", "stop", "--all", "--json"]).is_ok());
        assert!(parse(&["rathole-x", "service", "restart", "--json"]).is_ok());
        assert!(parse(&[
            "rathole-x",
            "service",
            "install",
            "server",
            "--yes",
            "--json"
        ])
        .is_ok());
        assert!(parse(&["rathole-x", "service", "uninstall", "--yes", "--json"]).is_ok());
        assert!(parse(&["rathole-x", "upgrade", "--yes", "--json"]).is_ok());
    }
    #[test]
    fn service_target_conflicts_and_install_role_are_parse_errors() {
        assert!(parse(&["rathole-x", "service", "install", "--yes"]).is_err());
        assert!(parse(&["rathole-x", "service", "start", "--all", "--name", "svc"]).is_err());
        assert!(parse(&[
            "rathole-x",
            "service",
            "uninstall",
            "--all",
            "--name",
            "svc",
            "--yes",
        ])
        .is_err());
        assert!(parse(&[
            "rathole-x",
            "service",
            "uninstall",
            "--all",
            "--config",
            "svc.toml",
            "--yes",
        ])
        .is_err());
        assert!(parse(&[
            "rathole-x",
            "service",
            "uninstall",
            "--all",
            "--purge",
            "--yes",
        ])
        .is_ok());
    }

    #[test]
    fn config_edit_config_and_name_conflict() {
        assert!(parse(&[
            "rathole-x",
            "config",
            "set",
            "--client",
            "--remote-addr",
            "host:2333",
            "--config",
            "client.toml",
            "--name",
            "client",
        ])
        .is_err());
        assert!(parse(&[
            "rathole-x",
            "config",
            "remove",
            "svc",
            "--config",
            "client.toml",
            "--name",
            "client",
        ])
        .is_err());
    }

    #[test]
    fn help_flag_follows_user_options() {
        let help = parse(&["rathole-x", "service", "install", "--help"])
            .unwrap_err()
            .to_string();
        assert!(help.find("--yes").unwrap() < help.find("-h, --help").unwrap());
    }
}
