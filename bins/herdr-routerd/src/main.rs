use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "herdr-routerd",
    version,
    about = "Router daemon for herdr-cross-session-messaging"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Start the daemon (detach if needed).
    Start,
    /// Stop the running daemon.
    Stop,
    /// Show daemon status.
    Status,
    /// Run one reconcile pass (used by plugin event hooks).
    Tick,
    /// Run the event loop in the foreground.
    Run,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    let _ = cli.command;
    anyhow::bail!("herdr-routerd is not implemented yet")
}
