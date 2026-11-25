use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "herdr-pool",
    version,
    about = "Peer-to-peer messaging for coding agents inside Herdr"
)]
struct Cli {
    /// Emit machine-readable JSON output.
    #[arg(long, global = true)]
    json: bool,
    /// Path to the Herdr socket (overrides HERDR_SOCKET_PATH).
    #[arg(long, global = true)]
    socket_path: Option<String>,
    /// Plugin state directory override.
    #[arg(long, global = true)]
    state_dir: Option<String>,
    /// Verbose logging.
    #[arg(short, long, global = true)]
    verbose: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let _ = cli;
    anyhow::bail!("herdr-pool is not implemented yet")
}
