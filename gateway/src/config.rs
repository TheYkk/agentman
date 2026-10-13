//! Gateway configuration loaded from TOML.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Interactive shell/session configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellConfig {
    /// How interactive shells are started.
    pub mode: ShellMode,

    /// Tmux session name to attach/create when `mode = "tmux"`.
    ///
    /// The session lives inside each agent container and enables reconnect/resume.
    pub tmux_session: String,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            mode: ShellMode::Tmux,
            tmux_session: "agentman".to_string(),
        }
    }
}

/// How to start an interactive shell when the user connects.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellMode {
    /// Start a plain login shell (`bash -l`).
    Bash,
    /// Attach to (or create) a persistent tmux session.
    Tmux,
}

impl Default for ShellMode {
    fn default() -> Self {
        Self::Tmux
    }
}

/// OpenSSH agent forwarding configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentForwardingConfig {
    /// Allow `ForwardAgent` / `auth-agent-req@openssh.com`.
    ///
    /// Security note: any process inside the container can talk to your forwarded agent
    /// for the lifetime of the SSH connection (it cannot extract private keys, but it can
    /// request signatures). Enable only if you trust the remote environment.
    pub allow: bool,
}

impl Default for AgentForwardingConfig {
    fn default() -> Self {
        Self { allow: true }
    }
}

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

    /// OpenSSH agent forwarding configuration
    #[serde(default)]
    pub agent_forwarding: AgentForwardingConfig,

    /// Interactive shell/session configuration
    #[serde(default)]
    pub shell: ShellConfig,

    /// Container security configuration
    #[serde(default)]
    pub container_security: ContainerSecurityConfig,

    /// Running Docker inside each sandbox container
    #[serde(default)]
    pub docker_in_sandbox: DockerInSandboxConfig,

    /// Which runtime provisions sandboxes: Docker containers or Proxmox LXC containers.
    #[serde(default)]
    pub runtime: RuntimeKind,

    /// Proxmox VE settings (used when `runtime = "proxmox"`).
    #[serde(default)]
    pub proxmox: ProxmoxConfig,
}

/// Sandbox runtime backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    /// Docker containers via the local Docker daemon.
    #[default]
    Docker,
    /// LXC containers on a Proxmox VE node.
    Proxmox,
}

impl std::fmt::Display for RuntimeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Docker => f.write_str("docker"),
            Self::Proxmox => f.write_str("proxmox"),
        }
    }
}

/// Proxmox VE (LXC) runtime configuration.
///
/// The gateway must run as root on the Proxmox node that hosts the containers: lifecycle
/// operations go through the Proxmox VE API, while shells are attached with `lxc-attach`,
/// pause/resume use `lxc-freeze`/`lxc-unfreeze`, and the workspace bind mount is configured
/// with `pct` (Proxmox only lets `root@pam` configure bind mounts, which API tokens cannot do).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxmoxConfig {
    /// Base URL of the Proxmox VE API (without `/api2/json`).
    pub api_url: String,

    /// Node that hosts agent containers. Empty means the local hostname.
    pub node: String,

    /// API token id, e.g. `agentman@pve!gateway`.
    pub token_id: String,

    /// File containing the API token secret. The `AGENTMAN_PROXMOX_TOKEN_SECRET`
    /// environment variable takes precedence when set.
    pub token_secret_file: Option<PathBuf>,

    /// PEM CA certificate used to verify the API's TLS certificate.
    /// Defaults to the cluster CA that every Proxmox node has.
    pub ca_cert: Option<PathBuf>,

    /// Skip TLS verification of the API (only for testing).
    pub insecure_skip_tls_verify: bool,

    /// Container template volume, e.g. `local:vztmpl/agentman-base.tar.zst`.
    /// On Proxmox VE 9.1+ this can also be an OCI image archive.
    pub ostemplate: String,

    /// Storage for the container root filesystem.
    pub storage: String,

    /// Root filesystem size in GiB.
    pub rootfs_size_gb: u32,

    /// Memory in MiB when `container_security.memory_limit` is not set.
    pub default_memory_mb: u64,

    /// Swap in MiB.
    pub swap_mb: u64,

    /// Network bridge for `net0`.
    pub bridge: String,

    /// IPv4 config for `net0` (`dhcp` or CIDR like `10.0.0.5/24`).
    pub ip: String,

    /// Optional IPv4 gateway for `net0` (when `ip` is static).
    pub gateway: Option<String>,

    /// Optional resource pool to place containers in.
    pub pool: Option<String>,

    /// Create unprivileged containers (recommended).
    pub unprivileged: bool,

    /// Enable the `nesting` feature (needed by systemd in recent distros and by docker-in-LXC).
    pub nesting: bool,

    /// Enable the `keyctl` feature on unprivileged containers when Docker runs inside
    /// (`docker_in_sandbox.mode` other than `off`).
    pub keyctl: bool,

    /// Enable the `fuse` feature (for fuse-overlayfs or other FUSE filesystems inside).
    pub fuse: bool,

    /// Host UID/GID offset for unprivileged containers (Proxmox default: 100000).
    pub id_map_offset: u32,

    /// UID that shells run as inside the container.
    pub exec_uid: u32,

    /// GID that shells run as inside the container.
    pub exec_gid: u32,

    /// Give shells the exec user's supplementary groups from the container's `/etc/group`
    /// (e.g. `docker`, so the user can reach the Docker socket). Requires `setpriv` and a
    /// user with `exec_uid` inside the container.
    pub exec_init_groups: bool,

    /// Base environment for shells inside the container (`lxc-attach` starts from a clean env).
    pub exec_env: Vec<String>,

    /// Directory with the LXC tools (`lxc-attach`, `lxc-info`, `lxc-freeze`, `lxc-unfreeze`).
    pub lxc_bin_dir: PathBuf,

    /// Path to `pct`.
    pub pct_path: PathBuf,
}

impl Default for ProxmoxConfig {
    fn default() -> Self {
        Self {
            api_url: "https://127.0.0.1:8006".to_string(),
            node: String::new(),
            token_id: "agentman@pve!gateway".to_string(),
            token_secret_file: Some(PathBuf::from("/etc/agentman/proxmox-token")),
            ca_cert: Some(PathBuf::from("/etc/pve/pve-root-ca.pem")),
            insecure_skip_tls_verify: false,
            ostemplate: "local:vztmpl/agentman-base.tar".to_string(),
            storage: "local-lvm".to_string(),
            rootfs_size_gb: 16,
            default_memory_mb: 4096,
            swap_mb: 512,
            bridge: "vmbr0".to_string(),
            ip: "dhcp".to_string(),
            gateway: None,
            pool: None,
            unprivileged: true,
            nesting: true,
            keyctl: true,
            fuse: false,
            id_map_offset: 100_000,
            // Matches the user baked into the base image (see Dockerfile: USERNAME/USER_UID/USER_GID).
            exec_uid: 1000,
            exec_gid: 1000,
            exec_init_groups: true,
            exec_env: vec![
                "HOME=/home/agent".to_string(),
                "SHELL=/bin/bash".to_string(),
                "USER=agent".to_string(),
                "LOGNAME=agent".to_string(),
                "LANG=C.UTF-8".to_string(),
                "BUN_INSTALL=/usr/local/bun".to_string(),
                "GOPATH=/home/agent/go".to_string(),
                "JAVA_HOME=/home/agent/.sdkman/candidates/java/current".to_string(),
                "PATH=/home/agent/.cargo/bin:/home/agent/.local/bin:/home/agent/.local/share/mise/shims:/home/agent/go/bin:/usr/local/go/bin:/usr/local/bun/bin:/usr/local/node/bin:/home/agent/.sdkman/candidates/java/current/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
            ],
            lxc_bin_dir: PathBuf::from("/usr/bin"),
            pct_path: PathBuf::from("/usr/sbin/pct"),
        }
    }
}

impl ProxmoxConfig {
    /// Value for the container's `features` option, if any feature is enabled.
    /// `inner_docker` is whether the sandbox runs its own Docker daemon (which needs keyctl).
    pub fn features(&self, inner_docker: bool) -> Option<String> {
        let mut features = Vec::new();
        if self.nesting {
            features.push("nesting=1");
        }
        // keyctl only exists for unprivileged containers, and only Docker needs it.
        if self.keyctl && self.unprivileged && inner_docker {
            features.push("keyctl=1");
        }
        if self.fuse {
            features.push("fuse=1");
        }
        (!features.is_empty()).then(|| features.join(","))
    }

    /// UID/GID on the host that maps to the container's exec user.
    ///
    /// Used to make bind-mounted workspaces and sockets writable from inside the container.
    pub fn host_owner(&self) -> (u32, u32) {
        if self.unprivileged {
            (
                self.id_map_offset + self.exec_uid,
                self.id_map_offset + self.exec_gid,
            )
        } else {
            (self.exec_uid, self.exec_gid)
        }
    }
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
            agent_forwarding: AgentForwardingConfig::default(),
            shell: ShellConfig::default(),
            container_security: ContainerSecurityConfig::default(),
            docker_in_sandbox: DockerInSandboxConfig::default(),
            runtime: RuntimeKind::default(),
            proxmox: ProxmoxConfig::default(),
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

    /// Memory limit (e.g., "2g"). Omit for no limit.
    pub memory_limit: Option<String>,

    /// CPU limit (e.g., "1.5" for 1.5 CPUs). Omit for no limit.
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
            memory_limit: None,
            cpu_limit: None,
            use_seccomp: true,
        }
    }
}

/// Whether each sandbox gets its own Docker daemon. Shared by every sandbox backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DockerInSandboxMode {
    /// Enable when the backend can isolate it without extra privileges: the Docker backend
    /// needs the sysbox runtime, an LXC backend needs an unprivileged container with nesting.
    Auto,
    /// Always enable. The Docker backend uses `docker_isolation`; startup fails if that
    /// isolation is unavailable.
    On,
    /// No Docker daemon inside sandboxes.
    Off,
}

impl Default for DockerInSandboxMode {
    fn default() -> Self {
        Self::Auto
    }
}

/// How the Docker backend isolates a sandbox that runs its own Docker daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DockerIsolation {
    /// Run under the sysbox runtime (user-namespaced, no `--privileged`).
    Sysbox,
    /// Run with `--privileged`. Root in the sandbox is effectively root on the host;
    /// only use this for fully trusted users.
    Privileged,
}

impl Default for DockerIsolation {
    fn default() -> Self {
        Self::Sysbox
    }
}

/// Docker-in-sandbox configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DockerInSandboxConfig {
    /// Whether sandboxes run their own Docker daemon (see [`DockerInSandboxMode`]).
    pub mode: DockerInSandboxMode,

    /// Docker backend only: isolation used when the mode is `on`.
    pub docker_isolation: DockerIsolation,

    /// Docker backend only: name of the sysbox runtime registered with the host Docker daemon.
    pub sysbox_runtime: String,

    /// Docker backend only: keep the inner Docker data (`/var/lib/docker`: images, build cache,
    /// volumes) in a per-workspace named volume so it survives container recreation.
    /// LXC sandboxes keep it in their persistent root filesystem.
    pub persist_data: bool,
}

impl Default for DockerInSandboxConfig {
    fn default() -> Self {
        Self {
            mode: DockerInSandboxMode::Auto,
            docker_isolation: DockerIsolation::Sysbox,
            sysbox_runtime: "sysbox-runc".to_string(),
            persist_data: true,
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
    #[allow(dead_code)]
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

    /// UID/GID on the host that should own workspace directories and forwarded sockets.
    pub fn workspace_owner(&self) -> (u32, u32) {
        match self.runtime {
            // Matches the default user baked into the base image (see Dockerfile: USER_UID/USER_GID).
            RuntimeKind::Docker => (1000, 1000),
            RuntimeKind::Proxmox => self.proxmox.host_owner(),
        }
    }

    /// Get the workspace path for a given GitHub user and project.
    pub fn workspace_path(&self, github_user: &str, project: &str) -> PathBuf {
        self.workspace_root.join(github_user).join(project)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let content = include_str!("../examples/gateway.toml");
        let config: GatewayConfig = toml::from_str(content).unwrap();
        assert_eq!(config.runtime, RuntimeKind::Docker);
        assert_eq!(config.workspace_owner(), (1000, 1000));
        assert_eq!(config.proxmox.token_id, "agentman@pve!gateway");
    }

    #[test]
    fn proxmox_runtime_shifts_workspace_owner() {
        let config: GatewayConfig = toml::from_str("runtime = \"proxmox\"\n").unwrap();
        assert_eq!(config.runtime, RuntimeKind::Proxmox);
        assert_eq!(config.workspace_owner(), (101000, 101000));
    }

    #[test]
    fn docker_in_sandbox_defaults_to_auto() {
        let config: GatewayConfig = toml::from_str("").unwrap();
        assert_eq!(config.docker_in_sandbox.mode, DockerInSandboxMode::Auto);
        assert_eq!(config.docker_in_sandbox.docker_isolation, DockerIsolation::Sysbox);
        assert_eq!(config.docker_in_sandbox.sysbox_runtime, "sysbox-runc");
        assert!(config.docker_in_sandbox.persist_data);
    }

    #[test]
    fn docker_in_sandbox_parses_modes() {
        for (raw, mode) in [
            ("auto", DockerInSandboxMode::Auto),
            ("off", DockerInSandboxMode::Off),
            ("on", DockerInSandboxMode::On),
        ] {
            let config: GatewayConfig =
                toml::from_str(&format!("[docker_in_sandbox]\nmode = \"{raw}\"\n")).unwrap();
            assert_eq!(config.docker_in_sandbox.mode, mode);
        }

        let config: GatewayConfig = toml::from_str(
            "[docker_in_sandbox]\nmode = \"on\"\ndocker_isolation = \"privileged\"\n",
        )
        .unwrap();
        assert_eq!(config.docker_in_sandbox.docker_isolation, DockerIsolation::Privileged);
    }
}
