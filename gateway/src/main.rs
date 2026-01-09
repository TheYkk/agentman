//! Agentman SSH Gateway
//!
//! A Rust SSH server that authenticates users via GitHub SSH keys,
//! manages Docker containers per project, and supports port forwarding.

mod config;
mod docker;
mod github;
mod ssh;
mod state;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{info, Level};
use tracing_subscriber::EnvFilter;

use crate::config::GatewayConfig;
use crate::docker::ContainerManager;
use crate::github::GitHubKeyFetcher;
use crate::state::StateManager;

/// Agentman SSH Gateway - manages agent containers via SSH
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
    info!("  Docker image: {}", config.docker_image);
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

    // Initialize Docker container manager
    let container_manager = Arc::new(
        ContainerManager::new(config.clone(), state.clone())
            .await
            .context("Failed to initialize Docker container manager")?,
    );

    // Run SSH server
    ssh::run_server(config, state, container_manager, github_fetcher).await?;

    Ok(())
}
