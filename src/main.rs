use std::ffi::OsString;

use anyhow::Result;
use clap::{error::ErrorKind, Parser};
use rathole::{run, Cli};
use tokio::{signal, sync::broadcast};
use tracing_subscriber::EnvFilter;

fn argv_requests_json(args: &[OsString]) -> bool {
    args.iter().any(|arg| arg == "--json")
}

fn exit_after_parse_error(error: clap::Error, json: bool) -> ! {
    let human_help = matches!(
        error.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
    );
    if json && !human_help {
        println!(
            "{}",
            serde_json::json!({
                "ok": false,
                "error": { "message": error.to_string().trim() },
            })
        );
    }
    let code = if human_help { 0 } else { 2 };
    let _ = error.print();
    std::process::exit(code);
}

#[tokio::main]
async fn main() -> Result<()> {
    let raw_args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let args = Cli::try_parse()
        .unwrap_or_else(|error| exit_after_parse_error(error, argv_requests_json(&raw_args)));
    // The UAC elevation relay: the elevated child runs hidden and writes all
    // its output into a log file that the waiting parent replays.
    if let Some(path) = &args.elevated_log {
        rathole::platform::redirect_stdio_to_file(path);
    }
    if args.confirmed {
        // The elevation relay re-runs the command line in a hidden child
        // with no TTY; the parent's interactive confirmation carries over
        // through this flag (environment inheritance is not guaranteed
        // across the UAC boundary).
        std::env::set_var(rathole::CONFIRMED_ENV, "1");
    }
    let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);
    tokio::spawn(async move {
        // `docker stop`, `systemctl stop` and OpenRC's supervise-daemon all
        // deliver SIGTERM; catch it in addition to ctrl-c so containers and
        // services shut down cleanly instead of waiting for the SIGKILL
        // fallback.
        #[cfg(unix)]
        let terminated = {
            use tokio::signal::unix::{signal, SignalKind};
            match signal(SignalKind::terminate()) {
                Ok(mut sigterm) => {
                    async move {
                        let _ = sigterm.recv().await;
                    }
                }
                Err(e) => panic!("Failed to listen for the SIGTERM signal: {:?}", e),
            }
        };
        #[cfg(not(unix))]
        let terminated = std::future::pending::<()>();

        tokio::select! {
            r = signal::ctrl_c() => {
                if let Err(e) = r {
                    // Something really weird happened. So just panic
                    panic!("Failed to listen for the ctrl-c signal: {:?}", e);
                }
            }
            _ = terminated => {}
        }

        if let Err(e) = shutdown_tx.send(true) {
            // shutdown signal must be catched and handle properly
            // `rx` must not be dropped
            panic!("Failed to send shutdown signal: {:?}", e);
        }
    });

    #[cfg(feature = "console")]
    {
        console_subscriber::init();

        tracing::info!("console_subscriber enabled");
    }
    #[cfg(not(feature = "console"))]
    {
        let is_atty = atty::is(atty::Stream::Stdout);

        let level = "info"; // if RUST_LOG not present, use `info` level
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::from(level)),
            )
            .with_ansi(is_atty)
            .init();
    }

    run(args, shutdown_rx).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_argv_detects_json_before_clap_reports_an_error() {
        assert!(argv_requests_json(&[
            OsString::from("config"),
            OsString::from("list"),
            OsString::from("--json"),
            OsString::from("--unknown"),
        ]));
        assert!(!argv_requests_json(&[
            OsString::from("config"),
            OsString::from("list")
        ]));
    }
}
