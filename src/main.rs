use anyhow::Result;
use clap::Parser;
use rathole::{run, Cli};
use tokio::{signal, sync::broadcast};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Cli::parse();
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
        if let Err(e) = signal::ctrl_c().await {
            // Something really weird happened. So just panic
            panic!("Failed to listen for the ctrl-c signal: {:?}", e);
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


