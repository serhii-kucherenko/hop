use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use sideshift_core::config::Config;
use sideshift_core::daemon;
use sideshift_protocol::control::NodeRole;

#[derive(Debug, Parser)]
#[command(
    name = "sideshift",
    about = "SideShift daemon for cross-machine mouse and keyboard handoff"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, clap::Subcommand)]
enum Commands {
    /// Start the daemon in either server or client mode.
    Run {
        /// Path to SideShift JSON config.
        #[arg(long, default_value = "sideshift.json")]
        config: PathBuf,
        /// Override role from the config.
        #[arg(long)]
        role: Option<RoleArg>,
        /// Print one-way latency estimates at the client side.
        #[arg(long, default_value_t = false)]
        log_latency: bool,
    },
    /// Reserved command for future network latency benchmarks.
    Bench,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, ValueEnum)]
enum RoleArg {
    Server,
    Client,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run {
            config,
            role,
            log_latency,
        } => {
            let config = Config::from_json_path(&config)?;
            let override_role = role.map(|role| match role {
                RoleArg::Server => NodeRole::Server,
                RoleArg::Client => NodeRole::Client,
            });
            daemon::run(config, override_role, log_latency).await?;
        }
        Commands::Bench => {
            println!("bench hook: reserved for future active RTT/one-way latency probes");
        }
    }

    Ok(())
}
