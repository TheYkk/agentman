//! Docker container provisioning and management.
//!
//! Handles:
//! - Creating agent containers with unique names
//! - Bind-mounting persistent workspaces
//! - Applying security hardening
//! - Container lifecycle (start, stop, exec)

use anyhow::{anyhow, Context, Result};
use bollard::container::{
    Config, CreateContainerOptions,
    StartContainerOptions, InspectContainerOptions,
    ListContainersOptions,
};
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults, ResizeExecOptions};
use bollard::models::HostConfig;
use bollard::Docker;
use chrono::Utc;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tracing::{info, warn};

use crate::config::GatewayConfig;
use crate::state::{StateManager, WorkspaceInfo};

/// Docker container manager.
pub struct ContainerManager {
    docker: Docker,
    config: Arc<GatewayConfig>,
    state: Arc<StateManager>,
}

impl ContainerManager {
    /// Create a new container manager.
    pub async fn new(config: Arc<GatewayConfig>, state: Arc<StateManager>) -> Result<Self> {
        let docker = Docker::connect_with_local_defaults()
            .context("Failed to connect to Docker daemon")?;

        // Verify connection
        docker
            .ping()
            .await
            .context("Failed to ping Docker daemon")?;

        info!("Connected to Docker daemon");

        Ok(Self {
            docker,
            config,
            state,
        })
    }

    /// Get or create a container for the given user and project.
    ///
    /// Returns the container ID.
    pub async fn get_or_create_container(
        &self,
        github_user: &str,
        project: &str,
    ) -> Result<String> {
        // Check if we already have a container for this workspace
        if let Some(workspace) = self.state.get_workspace(github_user, project).await {
            // Check if container still exists and is usable
            if let Some(ref container_id) = workspace.container_id {
                if self.container_exists(container_id).await? {
                    // Ensure it's running
                    self.ensure_running(container_id).await?;
                    return Ok(container_id.clone());
                }
            }
            // Container doesn't exist anymore, need to recreate
            warn!(
                "Container {} no longer exists, recreating",
                workspace.container_name
            );
        }

        // Create new container
        self.create_container(github_user, project).await
    }

    /// Create a new container for the given user and project.
    async fn create_container(&self, github_user: &str, project: &str) -> Result<String> {
        let now = Utc::now();
        let date_str = now.format("%Y%m%d").to_string();
        let container_name = format!("{}-{}-{}", project, github_user, date_str);

        // Ensure unique name by adding suffix if needed
        let container_name = self.ensure_unique_name(&container_name).await?;

        info!(
            "Creating container {} for {}/{}",
            container_name, github_user, project
        );

        // Ensure workspace directory exists
        let workspace_path = self.config.workspace_path(github_user, project);
        tokio::fs::create_dir_all(&workspace_path)
            .await
            .with_context(|| {
                format!(
                    "Failed to create workspace directory: {}",
                    workspace_path.display()
                )
            })?;

        // Build container configuration
        let host_config = self.build_host_config(&workspace_path)?;
        let env = self.build_env(github_user, project, &container_name);

        let config = Config {
            image: Some(self.config.docker_image.clone()),
            hostname: Some(container_name.clone()),
            env: Some(env),
            host_config: Some(host_config),
            working_dir: Some("/workspace".to_string()),
            tty: Some(true),
            open_stdin: Some(true),
            attach_stdin: Some(true),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        };

        let options = CreateContainerOptions {
            name: container_name.clone(),
            platform: None,
        };

        let response = self
            .docker
            .create_container(Some(options), config)
            .await
            .with_context(|| format!("Failed to create container {}", container_name))?;

        let container_id = response.id;
        info!("Created container {} ({})", container_name, &container_id[..12]);

        // Start the container
        self.docker
            .start_container(&container_id, None::<StartContainerOptions<String>>)
            .await
            .with_context(|| format!("Failed to start container {}", container_name))?;

        info!("Started container {}", container_name);

        // Save workspace info
        let workspace_info = WorkspaceInfo {
            github_user: github_user.to_string(),
            project: project.to_string(),
            container_name: container_name.clone(),
            container_id: Some(container_id.clone()),
            created_at: now,
            host_workspace_path: workspace_path,
        };

        self.state.set_workspace(workspace_info).await?;

        Ok(container_id)
    }

    /// Build the HostConfig with security settings and mounts.
    fn build_host_config(&self, workspace_path: &Path) -> Result<HostConfig> {
        let security = &self.config.container_security;

        let mut host_config = HostConfig {
            // Bind mount the workspace
            binds: Some(vec![format!(
                "{}:/workspace",
                workspace_path.display()
            )]),

            // Add host.docker.internal for reverse port forwarding
            extra_hosts: Some(vec!["host.docker.internal:host-gateway".to_string()]),

            // Never run privileged
            privileged: Some(false),

            // No access to Docker socket
            // (binds is already set, so docker.sock won't be mounted)

            // Network settings
            network_mode: Some("bridge".to_string()),

            // Init process for proper signal handling
            init: Some(true),

            ..Default::default()
        };

        // Apply security settings
        if security.cap_drop_all {
            host_config.cap_drop = Some(vec!["ALL".to_string()]);
            if !security.cap_add.is_empty() {
                host_config.cap_add = Some(security.cap_add.clone());
            }
        }

        if security.no_new_privileges {
            host_config.security_opt = Some(vec!["no-new-privileges:true".to_string()]);
        }

        if security.readonly_rootfs {
            host_config.readonly_rootfs = Some(true);
            // Add tmpfs for common writable paths
            host_config.tmpfs = Some(HashMap::from([
                ("/tmp".to_string(), "rw,noexec,nosuid,size=1g".to_string()),
                ("/run".to_string(), "rw,noexec,nosuid,size=64m".to_string()),
                ("/var/tmp".to_string(), "rw,noexec,nosuid,size=256m".to_string()),
            ]));
        }

        if let Some(ref memory) = security.memory_limit {
            // Parse memory limit (e.g., "4g" -> bytes)
            host_config.memory = Some(parse_memory_limit(memory)?);
        }

        if let Some(cpu) = security.cpu_limit {
            // CPU quota in 100ns units (1 CPU = 100000)
            host_config.nano_cpus = Some((cpu * 1_000_000_000.0) as i64);
        }

        // Use default seccomp profile (don't set to unconfined)
        // The default Docker seccomp profile is already applied unless explicitly disabled

        Ok(host_config)
    }

    /// Build environment variables for the container.
    fn build_env(&self, github_user: &str, project: &str, container_name: &str) -> Vec<String> {
        vec![
            format!("GITHUB_USERNAME={}", github_user),
            format!("AGENTMAN_PROJECT={}", project),
            format!("AGENTMAN_CONTAINER_ID={}", container_name),
            "TERM=xterm-256color".to_string(),
        ]
    }

    /// Ensure the container name is unique by adding a suffix if needed.
    async fn ensure_unique_name(&self, base_name: &str) -> Result<String> {
        let mut name = base_name.to_string();
        let mut suffix = 0;

        loop {
            let filters: HashMap<String, Vec<String>> = HashMap::from([(
                "name".to_string(),
                vec![format!("^{}$", name)],
            )]);

            let options = ListContainersOptions {
                all: true,
                filters,
                ..Default::default()
            };

            let containers = self
                .docker
                .list_containers(Some(options))
                .await
                .context("Failed to list containers")?;

            if containers.is_empty() {
                return Ok(name);
            }

            suffix += 1;
            name = format!("{}-{}", base_name, suffix);

            if suffix > 100 {
                return Err(anyhow!("Could not find unique container name after 100 attempts"));
            }
        }
    }

    /// Check if a container exists.
    async fn container_exists(&self, container_id: &str) -> Result<bool> {
        match self
            .docker
            .inspect_container(container_id, None::<InspectContainerOptions>)
            .await
        {
            Ok(_) => Ok(true),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(false),
            Err(e) => Err(e).context("Failed to inspect container"),
        }
    }

    /// Ensure a container is running.
    async fn ensure_running(&self, container_id: &str) -> Result<()> {
        let info = self
            .docker
            .inspect_container(container_id, None::<InspectContainerOptions>)
            .await
            .context("Failed to inspect container")?;

        let running = info
            .state
            .as_ref()
            .and_then(|s| s.running)
            .unwrap_or(false);

        if !running {
            info!("Starting stopped container {}", container_id);
            self.docker
                .start_container(container_id, None::<StartContainerOptions<String>>)
                .await
                .context("Failed to start container")?;
        }

        Ok(())
    }

    /// Get the container's IP address on the bridge network.
    pub async fn get_container_ip(&self, container_id: &str) -> Result<String> {
        let info = self
            .docker
            .inspect_container(container_id, None::<InspectContainerOptions>)
            .await
            .context("Failed to inspect container")?;

        let ip = info
            .network_settings
            .as_ref()
            .and_then(|ns| ns.ip_address.as_ref())
            .filter(|ip| !ip.is_empty())
            .or_else(|| {
                info.network_settings
                    .as_ref()
                    .and_then(|ns| ns.networks.as_ref())
                    .and_then(|nets| nets.get("bridge"))
                    .and_then(|bridge| bridge.ip_address.as_ref())
                    .filter(|ip| !ip.is_empty())
            })
            .ok_or_else(|| anyhow!("Container has no IP address"))?;

        Ok(ip.clone())
    }

    /// Create an exec instance in the container.
    ///
    /// Returns the exec ID.
    pub async fn create_exec(
        &self,
        container_id: &str,
        cmd: Vec<String>,
        tty: bool,
        env: Option<Vec<String>>,
    ) -> Result<String> {
        let options = CreateExecOptions {
            cmd: Some(cmd),
            attach_stdin: Some(true),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            tty: Some(tty),
            env,
            working_dir: Some("/workspace".to_string()),
            ..Default::default()
        };

        let response = self
            .docker
            .create_exec(container_id, options)
            .await
            .context("Failed to create exec")?;

        Ok(response.id)
    }

    /// Start an exec instance and return the multiplexed stream.
    pub async fn start_exec(&self, exec_id: &str) -> Result<StartExecResults> {
        let options = StartExecOptions {
            detach: false,
            tty: true,
            output_capacity: None,
        };

        let results = self
            .docker
            .start_exec(exec_id, Some(options))
            .await
            .context("Failed to start exec")?;

        Ok(results)
    }

    /// Resize the exec TTY.
    pub async fn resize_exec(&self, exec_id: &str, width: u16, height: u16) -> Result<()> {
        let options = ResizeExecOptions {
            width,
            height,
        };

        self.docker
            .resize_exec(exec_id, options)
            .await
            .context("Failed to resize exec")?;

        Ok(())
    }

    /// Get a reference to the Docker client.
    pub fn docker(&self) -> &Docker {
        &self.docker
    }
}

/// Parse a memory limit string (e.g., "4g", "512m") to bytes.
fn parse_memory_limit(s: &str) -> Result<i64> {
    let s = s.trim().to_lowercase();
    let (num, mult) = if s.ends_with('g') {
        (s.trim_end_matches('g'), 1024 * 1024 * 1024)
    } else if s.ends_with('m') {
        (s.trim_end_matches('m'), 1024 * 1024)
    } else if s.ends_with('k') {
        (s.trim_end_matches('k'), 1024)
    } else {
        (s.as_str(), 1)
    };

    let num: i64 = num
        .parse()
        .with_context(|| format!("Invalid memory limit: {}", s))?;

    Ok(num * mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_memory_limit() {
        assert_eq!(parse_memory_limit("4g").unwrap(), 4 * 1024 * 1024 * 1024);
        assert_eq!(parse_memory_limit("512m").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory_limit("1024k").unwrap(), 1024 * 1024);
        assert_eq!(parse_memory_limit("1000").unwrap(), 1000);
        assert_eq!(parse_memory_limit("2G").unwrap(), 2 * 1024 * 1024 * 1024);
    }
}
