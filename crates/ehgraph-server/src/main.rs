use std::path::PathBuf;
use std::time::Duration;

use axum::serve;
use clap::{Parser, Subcommand};
use ehgraph_core::now_ms;
use ehgraph_server::{boot, Config};
use tokio::net::TcpListener;

#[derive(Parser)]
#[command(
    name = "ehgraph",
    version,
    about = "Event Horizon Graph topology database server"
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
    /// Seal and ship the change log. Refuses to start if the server already holds the lock.
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
            let instance = running.state.engine.identity.instance_id.clone();
            let domain = running.state.engine.identity.domain.clone();
            tracing::info!(%listen, instance_id = %instance, domain = %domain, "ehgraph serving");
            let sessions = running.state.engine.sessions.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let _ = sessions.sweep(now_ms());
                }
            });
            let interval = running.state.config.backup_interval_s;
            if interval > 0 {
                let store = running.state.engine.store.clone();
                let prefix = running.state.engine.backup_prefix.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(Duration::from_secs(interval)).await;
                        if let Err(err) = ehgraph_backup::ship(&store, &prefix, now_ms()) {
                            tracing::error!(%err, instance_id = %instance, "scheduled backup failed");
                        }
                    }
                });
            }
            let listener = TcpListener::bind(listen).await?;
            serve(listener, running.router)
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
        }
        Command::Backup { config } => {
            let cfg = Config::load(&config)?;
            let running = boot(cfg)?;
            let checkpoint = ehgraph_backup::ship(
                &running.state.engine.store,
                &running.state.engine.backup_prefix,
                now_ms(),
            )?;
            println!("{}", serde_json::to_string_pretty(&checkpoint)?);
        }
        Command::Restore { config } => {
            let cfg = Config::load(&config)?;
            let checkpoint = ehgraph_backup::restore(&cfg.backup_prefix, &cfg.data_dir)?;
            println!("{}", serde_json::to_string_pretty(&checkpoint)?);
        }
    }
    Ok(())
}
