//! Agentman SSH Gateway
//!
//! A Rust SSH server that authenticates users via GitHub SSH keys,
//! manages a sandbox per project (Docker containers or Proxmox LXC containers),
//! and supports port forwarding.

mod config;
mod docker;
mod gateway_control;
mod inner_docker;
mod github;
mod proxmox;
mod pty;
mod runtime;
mod ssh;
mod state;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{info, Level};
use tracing_subscriber::EnvFilter;

use crate::config::{GatewayConfig, RuntimeKind};
use crate::docker::ContainerManager;
use crate::github::GitHubKeyFetcher;
use crate::proxmox::ProxmoxManager;
use crate::runtime::Runtime;
use crate::state::StateManager;

/// Agentman SSH Gateway - manages agent containers (Docker or Proxmox LXC) via SSH
#[derive(Parser, Debug)]
#[command(name = "agentman-gateway", version, about)]
struct Cli {
    /// Path to configuration file
    #[arg(short, long, default_value = "/etc/agentman/gateway.toml")]
    config: PathBuf,

    /// Generate default configuration and exit
    #[arg(long)]
    generate_config: bool,

    /// Override listen address
    #[arg(short, long)]
    listen: Option<String>,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logging
    let filter = if cli.verbose {
        EnvFilter::new(Level::DEBUG.to_string())
    } else {
        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(Level::INFO.to_string()))
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    // Handle --generate-config
    if cli.generate_config {
        let config = GatewayConfig::default();
        let content = toml::to_string_pretty(&config)?;
        println!("{}", content);
        return Ok(());
    }

    // Load configuration
    let mut config = GatewayConfig::load_or_default(&cli.config)
        .with_context(|| format!("Failed to load config from {}", cli.config.display()))?;

    // Apply CLI overrides
    if let Some(listen) = cli.listen {
        config.listen_addr = listen;
    }

    // Ensure required directories exist
    config.ensure_dirs()?;

    info!("Starting agentman-gateway");
    info!("  Listen address: {}", config.listen_addr);
    info!("  Runtime: {}", config.runtime);
    match config.runtime {
        RuntimeKind::Docker => info!("  Docker image: {}", config.docker_image),
        RuntimeKind::Proxmox => info!("  LXC template: {}", config.proxmox.ostemplate),
    }
    info!("  Workspace root: {}", config.workspace_root.display());

    let config = Arc::new(config);

    // Load or create state
    let state = Arc::new(
        StateManager::load(config.state_file.clone())
            .await
            .context("Failed to load state")?,
    );

    info!("State loaded from {}", config.state_file.display());

    // Initialize GitHub key fetcher
    let github_fetcher = Arc::new(GitHubKeyFetcher::new());

    // Initialize the sandbox runtime
    let runtime = Arc::new(match config.runtime {
        RuntimeKind::Docker => Runtime::Docker(
            ContainerManager::new(config.clone(), state.clone())
                .await
                .context("Failed to initialize Docker container manager")?,
        ),
        RuntimeKind::Proxmox => Runtime::Proxmox(
            ProxmoxManager::new(config.clone(), state.clone())
                .await
                .context("Failed to initialize Proxmox LXC manager")?,
        ),
    });
    runtime.spawn_background();

    // Run SSH server
    ssh::run_server(config, state, runtime, github_fetcher).await?;

    Ok(())
}
