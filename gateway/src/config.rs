//! Gateway configuration loaded from TOML.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Main gateway configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayConfig {
    /// SSH server listen address (default: "0.0.0.0:2222")
    pub listen_addr: String,

    /// Docker image to use for agent containers
    pub docker_image: String,

    /// Root path for persistent workspaces
    pub workspace_root: PathBuf,

    /// Path to the state file (key cache, container mappings)
    pub state_file: PathBuf,

    /// Path to the SSH host key
    pub host_key_path: PathBuf,

    /// Bootstrap GitHub usernames for auto-matching keys
    #[serde(default)]
    pub bootstrap_github_users: Vec<String>,

    /// Port forwarding configuration
    #[serde(default)]
    pub port_forwarding: PortForwardingConfig,

    /// Container security configuration
    #[serde(default)]
    pub container_security: ContainerSecurityConfig,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        let data_dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("/var/lib"))
            .join("agentman");

        Self {
            listen_addr: "0.0.0.0:2222".to_string(),
            docker_image: "agentman-base:dev".to_string(),
            workspace_root: data_dir.join("workspaces"),
            state_file: data_dir.join("state.json"),
            host_key_path: data_dir.join("host_key"),
            bootstrap_github_users: Vec::new(),
            port_forwarding: PortForwardingConfig::default(),
            container_security: ContainerSecurityConfig::default(),
        }
    }
}

/// Port forwarding policy configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PortForwardingConfig {
    /// Allow local port forwarding (ssh -L)
    pub allow_local: bool,

    /// Allow remote port forwarding (ssh -R)
    pub allow_remote: bool,

    /// Allow binding on non-loopback addresses for -R (GatewayPorts style)
    pub allow_gateway_ports: bool,

    /// Allow forwarding to non-local destinations (beyond localhost/container)
    pub allow_nonlocal_destinations: bool,
}

impl Default for PortForwardingConfig {
    fn default() -> Self {
        Self {
            allow_local: true,
            allow_remote: true,
            allow_gateway_ports: false,
            allow_nonlocal_destinations: false,
        }
    }
}

/// Container security settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContainerSecurityConfig {
    /// Drop all Linux capabilities
    pub cap_drop_all: bool,

    /// Capabilities to add back (if cap_drop_all is true)
    #[serde(default)]
    pub cap_add: Vec<String>,

    /// Enable no-new-privileges
    pub no_new_privileges: bool,

    /// Use read-only root filesystem
    pub readonly_rootfs: bool,

    /// Memory limit (e.g., "2g")
    pub memory_limit: Option<String>,

    /// CPU quota (e.g., "1.5" for 1.5 CPUs)
    pub cpu_limit: Option<f64>,

    /// Use default seccomp profile
    pub use_seccomp: bool,
}

impl Default for ContainerSecurityConfig {
    fn default() -> Self {
        Self {
            cap_drop_all: true,
            cap_add: vec![
                // Minimal caps needed for normal operation
                "CHOWN".to_string(),
                "DAC_OVERRIDE".to_string(),
                "FOWNER".to_string(),
                "SETGID".to_string(),
                "SETUID".to_string(),
            ],
            no_new_privileges: true,
            readonly_rootfs: false, // Many tools need writable /tmp, /var, etc.
            memory_limit: Some("4g".to_string()),
            cpu_limit: Some(2.0),
            use_seccomp: true,
        }
    }
}

impl GatewayConfig {
    /// Load configuration from a TOML file.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        let config: Self = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;
        Ok(config)
    }

    /// Load configuration from a file, or return defaults if the file doesn't exist.
    pub fn load_or_default(path: &Path) -> Result<Self> {
        if path.exists() {
            Self::load(path)
        } else {
            Ok(Self::default())
        }
    }

    /// Save configuration to a TOML file.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create config directory: {}", parent.display()))?;
        }
        let content = toml::to_string_pretty(self)
            .context("Failed to serialize config")?;
        std::fs::write(path, content)
            .with_context(|| format!("Failed to write config file: {}", path.display()))?;
        Ok(())
    }

    /// Ensure all required directories exist.
    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.workspace_root)
            .with_context(|| format!("Failed to create workspace root: {}", self.workspace_root.display()))?;

        if let Some(parent) = self.state_file.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create state directory: {}", parent.display()))?;
        }

        if let Some(parent) = self.host_key_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create host key directory: {}", parent.display()))?;
        }

        Ok(())
    }

    /// Get the workspace path for a given GitHub user and project.
    pub fn workspace_path(&self, github_user: &str, project: &str) -> PathBuf {
        self.workspace_root.join(github_user).join(project)
    }
}
