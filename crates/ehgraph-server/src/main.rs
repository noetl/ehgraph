use std::path::PathBuf;
use std::time::Duration;

use axum::serve;
use clap::{Parser, Subcommand};
use ehgraph_server::{boot, Config};
use tokio::net::TcpListener;

#[derive(Parser)]
#[command(
    name = "ehgraph",
    version,
    about = "Event Horizon Graph topology database server (standalone mode)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve one instance. Holds the data-directory lock until exit.
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
    /// Seal and ship the change log. Refuses to start if a server (or an embedding
    /// host) already holds the lock.
    Backup {
        #[arg(long)]
        config: PathBuf,
    },
    /// Restore a backup into an empty data directory, then exit.
    Restore {
        #[arg(long)]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ehgraph=info".into()),
        )
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::Serve { config } => {
            let cfg = Config::load(&config)?;
            let listen = cfg.listen_addr()?;
            let running = boot(cfg)?;
            let identity = running.graph.identity().clone();
            tracing::info!(%listen, instance_id = %identity.instance_id, domain = %identity.domain, "ehgraph serving");
            running.graph.start_maintenance()?;
            let listener = TcpListener::bind(listen).await?;
            serve(listener, running.router)
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
            running.graph.shutdown(Duration::from_secs(10)).await;
        }
        Command::Backup { config } => {
            let cfg = Config::load(&config)?;
            let running = boot(cfg)?;
            let checkpoint = running.graph.backup_now()?;
            println!("{}", serde_json::to_string_pretty(&checkpoint)?);
        }
        Command::Restore { config } => {
            let cfg = Config::load(&config)?;
            let checkpoint = ehgraph::restore(&cfg.graph.backup_prefix, &cfg.graph.data_dir)?;
            println!("{}", serde_json::to_string_pretty(&checkpoint)?);
        }
    }
    Ok(())
}
