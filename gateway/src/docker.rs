//! Docker container provisioning and management.
//!
//! Handles:
//! - Creating agent containers with unique names
//! - Bind-mounting persistent workspaces
//! - Applying security hardening
//! - Container lifecycle (start, stop, exec)

use anyhow::{anyhow, Context, Result};
use bollard::exec::{CreateExecOptions, ResizeExecOptions, StartExecOptions, StartExecResults};
use bollard::models::{ContainerCreateBody, HostConfig};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, InspectContainerOptions, ListContainersOptionsBuilder,
    RemoveContainerOptionsBuilder, StartContainerOptions, StopContainerOptionsBuilder,
};
use bollard::Docker;
use chrono::Utc;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Command;
use tracing::{info, warn};

use crate::config::GatewayConfig;
use crate::state::{StateManager, WorkspaceInfo};

/// Options for destroying a workspace (container(s) + persistent data).
#[derive(Debug, Clone, Copy)]
pub struct DestroyOptions {
    /// If true, do not delete the persistent workspace directory on the host.
    pub keep_workspace: bool,
    /// If true, force-remove containers (kill if needed).
    pub force: bool,
    /// If true, print what would happen but do not actually delete anything.
    pub dry_run: bool,
}

/// Summary of a destroy operation.
#[derive(Debug, Clone)]
pub struct DestroyResult {
    pub removed_containers: Vec<String>,
    pub workspace_path: PathBuf,
    pub workspace_deleted: bool,
    pub state_entry_deleted: bool,
    pub warnings: Vec<String>,
}

impl DestroyResult {
    pub fn format_human(&self) -> String {
        let mut out = String::new();

        out.push_str("agentman: destroy summary\n");
        out.push_str(&format!(
            "- removed containers: {}\n",
            if self.removed_containers.is_empty() {
                "(none)".to_string()
            } else {
                self.removed_containers.join(", ")
            }
        ));
        out.push_str(&format!(
            "- workspace path: {}\n",
            self.workspace_path.display()
        ));
        out.push_str(&format!(
            "- workspace deleted: {}\n",
            if self.workspace_deleted { "yes" } else { "no" }
        ));
        out.push_str(&format!(
            "- state entry deleted: {}\n",
            if self.state_entry_deleted { "yes" } else { "no" }
        ));

        if !self.warnings.is_empty() {
            out.push_str("- warnings:\n");
            for w in &self.warnings {
                out.push_str(&format!("  - {w}\n"));
            }
        }

        out
    }
}

#[cfg(unix)]
async fn ensure_workspace_writable(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    // This matches the default user baked into the base image (see Dockerfile: USER_UID/USER_GID).
    // If you run a custom image with a different UID/GID, you may need to adjust this logic.
    const CONTAINER_UID: u32 = 1000;
    const CONTAINER_GID: u32 = 1000;

    // Ensure directory exists.
    tokio::fs::create_dir_all(path)
        .await
        .with_context(|| format!("Failed to create workspace directory: {}", path.display()))?;

    let md = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("Failed to stat workspace directory: {}", path.display()))?;
    let mode = md.permissions().mode() & 0o777;

    // Check whether UID 1000 can write to this directory with current ownership/mode.
    let writable = if md.uid() == CONTAINER_UID {
        (mode & 0o200 != 0) && (mode & 0o100 != 0)
    } else if md.gid() == CONTAINER_GID {
        (mode & 0o020 != 0) && (mode & 0o010 != 0)
    } else {
        (mode & 0o002 != 0) && (mode & 0o001 != 0)
    };
    if writable {
        return Ok(());
    }

    // Try to fix ownership and mode. This will succeed when the gateway runs as root.
    // We do NOT do recursive chown to avoid expensive walks on large workspaces.
    // The key requirement for editor bootstraps is that the workspace root is writable.
    match Command::new("chown")
        .arg(format!("{CONTAINER_UID}:{CONTAINER_GID}"))
        .arg(path)
        .status()
        .await
    {
        Ok(status) if status.success() => {}
        Ok(status) => warn!(
            "chown {}:{} {} exited with status {}",
            CONTAINER_UID,
            CONTAINER_GID,
            path.display(),
            status
        ),
        Err(e) => warn!(
            "Failed to run chown {}:{} {}: {}",
            CONTAINER_UID,
            CONTAINER_GID,
            path.display(),
            e
        ),
    }

    // Prefer 0775 (not world-writable) after chown.
    let _ = tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o775)).await;

    // If it’s still not writable, fall back to 0777 (best-effort to make editors work).
    let md2 = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("Failed to stat workspace directory: {}", path.display()))?;
    let mode2 = md2.permissions().mode() & 0o777;
    let writable2 = if md2.uid() == CONTAINER_UID {
        (mode2 & 0o200 != 0) && (mode2 & 0o100 != 0)
    } else if md2.gid() == CONTAINER_GID {
        (mode2 & 0o020 != 0) && (mode2 & 0o010 != 0)
    } else {
        (mode2 & 0o002 != 0) && (mode2 & 0o001 != 0)
    };
    if !writable2 {
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o777))
            .await
            .with_context(|| {
                format!(
                    "Failed to chmod workspace directory to be writable: {}",
                    path.display()
                )
            })?;
    }

    Ok(())
}

#[cfg(not(unix))]
async fn ensure_workspace_writable(_path: &Path) -> Result<()> {
    Ok(())
}

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
        // Ensure the host workspace directory is writable by the container user (needed for Zed/VS Code bootstraps).
        let workspace_path = self.config.workspace_path(github_user, project);
        ensure_workspace_writable(&workspace_path).await?;

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
        ensure_workspace_writable(&workspace_path).await?;

        let labels: HashMap<String, String> = HashMap::from([
            ("agentman.managed".to_string(), "true".to_string()),
            ("agentman.github_user".to_string(), github_user.to_string()),
            ("agentman.project".to_string(), project.to_string()),
            (
                "agentman.workspace_path".to_string(),
                workspace_path.display().to_string(),
            ),
        ]);

        // Build container configuration
        let host_config = self.build_host_config(&workspace_path)?;
        let env = self.build_env(github_user, project, &container_name);

        let config = ContainerCreateBody {
            image: Some(self.config.docker_image.clone()),
            hostname: Some(container_name.clone()),
            env: Some(env),
            labels: Some(labels),
            host_config: Some(host_config),
            working_dir: Some("/workspace".to_string()),
            tty: Some(true),
            open_stdin: Some(true),
            attach_stdin: Some(true),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        };

        let options = CreateContainerOptionsBuilder::new()
            .name(&container_name)
            .build();

        let response = self
            .docker
            .create_container(Some(options), config)
            .await
            .with_context(|| format!("Failed to create container {}", container_name))?;

        let container_id = response.id;
        info!("Created container {} ({})", container_name, &container_id[..12]);

        // Start the container
        self.docker
            .start_container(&container_id, None::<StartContainerOptions>)
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

            let options = ListContainersOptionsBuilder::new()
                .all(true)
                .filters(&filters)
                .build();

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
        let paused = info
            .state
            .as_ref()
            .and_then(|s| s.paused)
            .unwrap_or(false);

        // If a container is paused, it will appear "running" but exec will be unusable.
        // Unpause it so users can reconnect cleanly.
        if paused {
            info!("Unpausing paused container {}", container_id);
            self.docker
                .unpause_container(container_id)
                .await
                .context("Failed to unpause container")?;
        }

        if !running {
            info!("Starting stopped container {}", container_id);
            self.docker
                .start_container(container_id, None::<StartContainerOptions>)
                .await
                .context("Failed to start container")?;
        }

        Ok(())
    }

    /// List all workspaces for a given GitHub user.
    pub async fn list_workspaces(&self, github_user: &str) -> Vec<WorkspaceInfo> {
        self.state.list_workspaces(github_user).await
    }

    /// Get workspace info by (github_user, project).
    pub async fn get_workspace(&self, github_user: &str, project: &str) -> Option<WorkspaceInfo> {
        self.state.get_workspace(github_user, project).await
    }

    /// Get the container's IP address on the bridge network.
    ///
    /// Not currently used in the gateway, but kept for future port-forwarding / networking features.
    #[allow(dead_code)]
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
    pub async fn start_exec(&self, exec_id: &str, tty: bool) -> Result<StartExecResults> {
        let options = StartExecOptions {
            detach: false,
            tty,
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

    /// Destroy a workspace:
    /// - Stop/remove any managed container(s) for (github_user, project)
    /// - Optionally delete the persistent workspace directory on the host
    /// - Remove the workspace entry from the gateway state file
    pub async fn destroy_workspace(
        &self,
        github_user: &str,
        project: &str,
        opts: DestroyOptions,
    ) -> Result<DestroyResult> {
        let mut warnings = Vec::new();

        // Workspace path is derived from config (safe and deterministic).
        let workspace_path = self.config.workspace_path(github_user, project);

        // Collect targets:
        // - state-mapped container id/name (works even for older containers without labels)
        // - any currently running/stopped containers labeled as managed for this workspace
        let mut targets: Vec<String> = Vec::new();
        if let Some(ws) = self.state.get_workspace(github_user, project).await {
            if let Some(id) = ws.container_id {
                targets.push(id);
            }
            // Removing by name also works if the ID is stale/missing.
            targets.push(ws.container_name);
        }

        // Add labeled containers (newer containers).
        match self.list_labeled_workspace_containers(github_user, project).await {
            Ok(mut ids) => targets.append(&mut ids),
            Err(e) => warnings.push(format!("failed to list labeled containers: {e}")),
        }

        // Deduplicate targets.
        targets.sort();
        targets.dedup();

        let mut removed_containers = Vec::new();

        for target in targets {
            if opts.dry_run {
                removed_containers.push(format!("{target} (dry-run)"));
                continue;
            }

            // Best-effort stop first (unless forced).
            if !opts.force {
                match self
                    .docker
                    .stop_container(
                        &target,
                        Some(StopContainerOptionsBuilder::new().t(10).build()),
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 404, ..
                    }) => {
                        // Already gone.
                    }
                    Err(e) => {
                        warnings.push(format!("stop container {target}: {e}"));
                    }
                }
            }

            let rm_opts = RemoveContainerOptionsBuilder::new()
                .force(opts.force)
                .v(true)
                .link(false)
                .build();

            match self.docker.remove_container(&target, Some(rm_opts)).await {
                Ok(_) => {
                    removed_containers.push(target);
                }
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => {
                    // Not found; ignore.
                }
                Err(e) => {
                    warnings.push(format!("remove container {target}: {e}"));
                }
            }
        }

        // Delete persistent workspace directory.
        let mut workspace_deleted = false;
        if !opts.keep_workspace {
            if opts.dry_run {
                if workspace_path.exists() {
                    workspace_deleted = true;
                }
            } else if workspace_path.exists() {
                tokio::fs::remove_dir_all(&workspace_path)
                    .await
                    .with_context(|| {
                        format!(
                            "Failed to delete workspace directory: {}",
                            workspace_path.display()
                        )
                    })?;
                workspace_deleted = true;
            }
        }

        // Remove the workspace entry from state.
        let state_entry_deleted = if opts.dry_run {
            false
        } else {
            self.state
                .remove_workspace(github_user, project)
                .await?
                .is_some()
        };

        Ok(DestroyResult {
            removed_containers,
            workspace_path,
            workspace_deleted,
            state_entry_deleted,
            warnings,
        })
    }

    async fn list_labeled_workspace_containers(
        &self,
        github_user: &str,
        project: &str,
    ) -> Result<Vec<String>> {
        let filters: HashMap<String, Vec<String>> = HashMap::from([(
            "label".to_string(),
            vec!["agentman.managed=true".to_string()],
        )]);

        let options = ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&filters)
            .build();

        let containers = self
            .docker
            .list_containers(Some(options))
            .await
            .context("Failed to list containers")?;

        let mut out = Vec::new();
        for c in containers {
            let labels = c.labels.unwrap_or_default();
            let matches = labels.get("agentman.github_user").map(|v| v.as_str()) == Some(github_user)
                && labels.get("agentman.project").map(|v| v.as_str()) == Some(project);
            if !matches {
                continue;
            }
            if let Some(id) = c.id {
                out.push(id);
            }
        }
        Ok(out)
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
