//! Docker container provisioning and management.
//!
//! Handles:
//! - Creating agent containers with unique names
//! - Bind-mounting persistent workspaces
//! - Applying security hardening
//! - Container lifecycle (start, stop, exec)
//! - Optional Docker daemon inside each sandbox (sysbox or privileged)

use anyhow::{anyhow, Context, Result};
use bollard::container::LogOutput;
use bollard::errors::Error as BollardError;
use bollard::exec::{CreateExecOptions, ResizeExecOptions, StartExecOptions, StartExecResults};
use bollard::models::{ContainerCreateBody, HostConfig};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, InspectContainerOptions, ListContainersOptionsBuilder,
    RemoveContainerOptionsBuilder, RemoveVolumeOptionsBuilder, StartContainerOptions,
    StatsOptionsBuilder,
    StopContainerOptionsBuilder,
};
use bollard::Docker;
use chrono::{DateTime, Utc};
use futures::{FutureExt, StreamExt};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{info, warn};

use crate::config::{DockerInSandboxMode, DockerIsolation, GatewayConfig, RuntimeKind};
use crate::inner_docker;
use crate::runtime::{
    KeyedLocks, DestroyOptions, DestroyResult, ExecOutput, ExecProcess, ExecSpec, Resizer, SandboxStats,
    SandboxStatus,
};
use crate::state::{StateManager, WorkspaceInfo};


/// Container label recording which inner-Docker mode a sandbox was created with.
const INNER_DOCKER_LABEL: &str = "agentman.inner_docker";

/// Inner-Docker mode resolved against what the host Docker daemon supports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InnerDocker {
    Off,
    Sysbox { runtime: String },
    Privileged,
}

impl InnerDocker {
    /// Resolve the configured mode given the runtimes the host daemon reports.
    fn resolve(
        mode: DockerInSandboxMode,
        isolation: DockerIsolation,
        sysbox_runtime: &str,
        has_runtime: impl Fn(&str) -> bool,
    ) -> Result<Self> {
        let sysbox = || Self::Sysbox {
            runtime: sysbox_runtime.to_string(),
        };
        match (mode, isolation) {
            (DockerInSandboxMode::Off, _) => Ok(Self::Off),
            // `auto` never escalates to privileged; it only uses sysbox when it is installed.
            (DockerInSandboxMode::Auto, _) if has_runtime(sysbox_runtime) => Ok(sysbox()),
            (DockerInSandboxMode::Auto, _) => Ok(Self::Off),
            (DockerInSandboxMode::On, DockerIsolation::Privileged) => Ok(Self::Privileged),
            (DockerInSandboxMode::On, DockerIsolation::Sysbox) if has_runtime(sysbox_runtime) => {
                Ok(sysbox())
            }
            (DockerInSandboxMode::On, DockerIsolation::Sysbox) => Err(anyhow!(
                "docker_in_sandbox.mode = \"on\" with docker_isolation = \"sysbox\", but the Docker \
                 daemon has no \"{}\" runtime; install sysbox (https://github.com/nestybox/sysbox), \
                 set docker_isolation = \"privileged\", or set mode = \"off\"",
                sysbox_runtime
            )),
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Sysbox { .. } => "sysbox",
            Self::Privileged => "privileged",
        }
    }
}

/// Name of the named volume holding a workspace's inner `/var/lib/docker`.
fn inner_docker_volume_name(github_user: &str, project: &str) -> String {
    format!("agentman-docker-{}-{}", github_user, project)
}

/// Ensure a host workspace directory exists and is writable by the container user, which
/// appears on the host as `owner` (UID, GID).
#[cfg(unix)]
pub async fn ensure_workspace_writable(path: &Path, owner: (u32, u32)) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let (container_uid, container_gid) = owner;

    // Ensure directory exists.
    tokio::fs::create_dir_all(path)
        .await
        .with_context(|| format!("Failed to create workspace directory: {}", path.display()))?;

    let md = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("Failed to stat workspace directory: {}", path.display()))?;
    let mode = md.permissions().mode() & 0o777;

    // Check whether UID 1000 can write to this directory with current ownership/mode.
    let writable = if md.uid() == container_uid {
        (mode & 0o200 != 0) && (mode & 0o100 != 0)
    } else if md.gid() == container_gid {
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
        .arg(format!("{container_uid}:{container_gid}"))
        .arg(path)
        .status()
        .await
    {
        Ok(status) if status.success() => {}
        Ok(status) => warn!(
            "chown {}:{} {} exited with status {}",
            container_uid,
            container_gid,
            path.display(),
            status
        ),
        Err(e) => warn!(
            "Failed to run chown {}:{} {}: {}",
            container_uid,
            container_gid,
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
    let writable2 = if md2.uid() == container_uid {
        (mode2 & 0o200 != 0) && (mode2 & 0o100 != 0)
    } else if md2.gid() == container_gid {
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
pub async fn ensure_workspace_writable(_path: &Path, _owner: (u32, u32)) -> Result<()> {
    Ok(())
}

/// Docker container manager.
pub struct ContainerManager {
    docker: Docker,
    config: Arc<GatewayConfig>,
    state: Arc<StateManager>,
    inner_docker: InnerDocker,
    create_locks: KeyedLocks,
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

        let dind = &config.docker_in_sandbox;
        let inner_docker = if dind.mode == DockerInSandboxMode::Off {
            InnerDocker::Off
        } else {
            let runtimes = docker
                .info()
                .await
                .context("Failed to query Docker daemon info")?
                .runtimes
                .unwrap_or_default();
            InnerDocker::resolve(dind.mode, dind.docker_isolation, &dind.sysbox_runtime, |r| {
                runtimes.contains_key(r)
            })?
        };
        match &inner_docker {
            InnerDocker::Off => info!("Docker inside sandboxes: off"),
            InnerDocker::Sysbox { runtime } => {
                info!("Docker inside sandboxes: sysbox (runtime {})", runtime)
            }
            InnerDocker::Privileged => {
                warn!("Docker inside sandboxes: privileged (sandbox root is host root)")
            }
        }

        Ok(Self {
            docker,
            config,
            state,
            inner_docker,
            create_locks: KeyedLocks::default(),
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
        ensure_workspace_writable(&workspace_path, self.config.workspace_owner()).await?;

        // A second connection waits for an in-flight create (e.g. a slow image unpack) instead
        // of racing it for the same container name.
        let _create_guard = self
            .create_locks
            .lock(&format!("{github_user}/{project}"))
            .await;

        // Check if we already have a container for this workspace
        if let Some(workspace) = self.state.get_workspace(github_user, project).await {
            // Check if container still exists and is usable
            if workspace.runtime != RuntimeKind::Docker {
                warn!(
                    "Workspace {}/{} belongs to the {} runtime; creating a Docker container",
                    github_user, project, workspace.runtime
                );
            } else if let Some(ref container_id) = workspace.container_id {
                if self.container_exists(container_id).await? {
                    // Ensure it's running
                    self.ensure_running(container_id).await?;
                    self.ensure_inner_dockerd(container_id).await;
                    return Ok(container_id.clone());
                }
            }
            // Container doesn't exist anymore, need to recreate
            warn!(
                "Container {} no longer exists, recreating",
                workspace.container_name
            );
        }

        // A create that outlived its session or the gateway may have left a container behind
        // that the state file never recorded; reuse it rather than reserving a new name.
        if let Some(found) = self.find_untracked_container(github_user, project).await? {
            return self.adopt_container(github_user, project, found).await;
        }

        // Create new container
        self.create_container(github_user, project).await
    }

    /// The newest managed container labeled with this workspace, if any: (id, name, created).
    async fn find_untracked_container(
        &self,
        github_user: &str,
        project: &str,
    ) -> Result<Option<(String, String, Option<i64>)>> {
        let filters: HashMap<String, Vec<String>> = HashMap::from([(
            "label".to_string(),
            vec![
                "agentman.managed=true".to_string(),
                format!("agentman.github_user={}", github_user),
                format!("agentman.project={}", project),
            ],
        )]);
        let options = ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&filters)
            .build();
        let mut containers = self
            .docker
            .list_containers(Some(options))
            .await
            .context("Failed to list containers")?;
        containers.sort_by_key(|c| std::cmp::Reverse(c.created.unwrap_or(0)));
        Ok(containers.into_iter().find_map(|c| {
            let id = c.id?;
            let name = c
                .names
                .and_then(|n| n.into_iter().next())
                .map(|n| n.trim_start_matches('/').to_string())
                .unwrap_or_else(|| id.clone());
            Some((id, name, c.created))
        }))
    }

    /// Start a container found by [`Self::find_untracked_container`] and record it in state.
    async fn adopt_container(
        &self,
        github_user: &str,
        project: &str,
        (container_id, container_name, created): (String, String, Option<i64>),
    ) -> Result<String> {
        warn!(
            "Reusing untracked container {} for {}/{}",
            container_name, github_user, project
        );
        self.ensure_running(&container_id).await?;
        self.ensure_inner_dockerd(&container_id).await;
        self.state
            .set_workspace(WorkspaceInfo {
                github_user: github_user.to_string(),
                project: project.to_string(),
                runtime: RuntimeKind::Docker,
                container_name,
                container_id: Some(container_id.clone()),
                created_at: created
                    .and_then(|t| DateTime::from_timestamp(t, 0))
                    .unwrap_or_else(Utc::now),
                host_workspace_path: self.config.workspace_path(github_user, project),
            })
            .await?;
        Ok(container_id)
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
        ensure_workspace_writable(&workspace_path, self.config.workspace_owner()).await?;

        let labels: HashMap<String, String> = HashMap::from([
            ("agentman.managed".to_string(), "true".to_string()),
            (
                INNER_DOCKER_LABEL.to_string(),
                self.inner_docker.label().to_string(),
            ),
            ("agentman.github_user".to_string(), github_user.to_string()),
            ("agentman.project".to_string(), project.to_string()),
            (
                "agentman.workspace_path".to_string(),
                workspace_path.display().to_string(),
            ),
        ]);

        // Build container configuration
        let data_volume = (self.inner_docker != InnerDocker::Off
            && self.config.docker_in_sandbox.persist_data)
            .then(|| inner_docker_volume_name(github_user, project));
        let host_config = build_host_config(
            &self.config,
            &self.inner_docker,
            &workspace_path,
            data_volume.as_deref(),
        )?;
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

        let response = match self.docker.create_container(Some(options), config).await {
            Ok(r) => r,
            // The name is still reserved by a create the daemon has not finished (e.g. one left
            // over from before a gateway restart). Wait for that container to appear and reuse it.
            Err(BollardError::DockerResponseServerError {
                status_code: 409, ..
            }) => {
                warn!(
                    "Container name {} is reserved by an unfinished create; waiting for it",
                    container_name
                );
                for _ in 0..60 {
                    if let Some(found) = self.find_untracked_container(github_user, project).await? {
                        return self.adopt_container(github_user, project, found).await;
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                return Err(anyhow!(
                    "Container name {} is still reserved by an unfinished create; remove it with `docker rm -f {}`",
                    container_name,
                    container_name
                ));
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("Failed to create container {}", container_name));
            }
        };

        let container_id = response.id;
        info!("Created container {} ({})", container_name, &container_id[..12]);

        // Start the container
        self.docker
            .start_container(&container_id, None::<StartContainerOptions>)
            .await
            .with_context(|| format!("Failed to start container {}", container_name))?;

        info!("Started container {}", container_name);

        self.ensure_inner_dockerd(&container_id).await;

        // Save workspace info
        let workspace_info = WorkspaceInfo {
            github_user: github_user.to_string(),
            project: project.to_string(),
            runtime: RuntimeKind::Docker,
            container_name: container_name.clone(),
            container_id: Some(container_id.clone()),
            created_at: now,
            host_workspace_path: workspace_path,
        };

        self.state.set_workspace(workspace_info).await?;

        Ok(container_id)
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

    /// Start the inner Docker daemon if this container was created with Docker-in-sandbox.
    ///
    /// Best-effort: a failure is logged and the session continues without Docker.
    async fn ensure_inner_dockerd(&self, container_id: &str) {
        let enabled = match self
            .docker
            .inspect_container(container_id, None::<InspectContainerOptions>)
            .await
        {
            Ok(info) => info
                .config
                .and_then(|c| c.labels)
                .and_then(|l| l.get(INNER_DOCKER_LABEL).cloned())
                .is_some_and(|mode| mode != "off"),
            Err(e) => {
                warn!("Failed to inspect container {}: {}", container_id, e);
                return;
            }
        };
        if !enabled {
            return;
        }

        let options = CreateExecOptions {
            cmd: Some(inner_docker::start_dockerd_command(self.config.workspace_owner().1)),
            user: Some("root".to_string()),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        };
        // The command returns once dockerd answers, so wait for it: the session's first
        // `docker` command then finds a ready daemon.
        let run = async {
            let exec = self.docker.create_exec(container_id, options).await?;
            let mut stderr = Vec::new();
            if let StartExecResults::Attached { mut output, .. } =
                self.docker.start_exec(&exec.id, None::<StartExecOptions>).await?
            {
                while let Some(chunk) = output.next().await {
                    if let LogOutput::StdErr { message } = chunk? {
                        stderr.extend_from_slice(&message);
                    }
                }
            }
            let code = self
                .docker
                .inspect_exec(&exec.id)
                .await?
                .exit_code
                .unwrap_or(0);
            Ok::<_, bollard::errors::Error>((code, stderr))
        };
        let limit = Duration::from_secs(u64::from(inner_docker::DOCKERD_READY_TIMEOUT_SECS) + 15);
        match timeout(limit, run).await {
            Ok(Ok((0, _))) => {}
            Ok(Ok((code, stderr))) => warn!(
                "dockerd in container {} is not ready (exit {}): {}",
                container_id,
                code,
                String::from_utf8_lossy(&stderr).trim()
            ),
            Ok(Err(e)) => warn!("Failed to start dockerd in container {}: {}", container_id, e),
            Err(_) => warn!("Timed out starting dockerd in container {}", container_id),
        }
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

    /// Start a process in the container with its I/O attached.
    pub async fn exec(&self, container_id: &str, spec: ExecSpec) -> Result<ExecProcess> {
        let options = CreateExecOptions {
            cmd: Some(spec.cmd),
            attach_stdin: Some(true),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            tty: Some(spec.tty),
            env: Some(spec.env),
            working_dir: Some("/workspace".to_string()),
            ..Default::default()
        };

        let exec_id = self
            .docker
            .create_exec(container_id, options)
            .await
            .context("Failed to create exec")?
            .id;

        let results = self
            .docker
            .start_exec(
                &exec_id,
                Some(StartExecOptions {
                    detach: false,
                    tty: spec.tty,
                    output_capacity: None,
                }),
            )
            .await
            .context("Failed to start exec")?;

        let (output, input) = match results {
            StartExecResults::Attached { output, input } => (output, input),
            StartExecResults::Detached => {
                return Err(anyhow!("Exec started in detached mode unexpectedly"));
            }
        };

        let resizer = Resizer::Docker {
            docker: self.docker.clone(),
            exec_id: exec_id.clone(),
        };
        if let (true, Some((cols, rows))) = (spec.tty, spec.size) {
            if let Err(e) = resizer.resize(cols, rows).await {
                warn!("Failed to set initial exec size: {}", e);
            }
        }

        let output = output
            .map(|item| match item {
                Ok(LogOutput::StdErr { message }) => Ok(ExecOutput::Stderr(message)),
                Ok(LogOutput::StdOut { message })
                | Ok(LogOutput::StdIn { message })
                | Ok(LogOutput::Console { message }) => Ok(ExecOutput::Stdout(message)),
                Err(e) => Err(std::io::Error::other(e)),
            })
            .boxed();

        let docker = self.docker.clone();
        let exit_status = async move {
            // `inspect_exec` may briefly report Running=true even after the output stream ends,
            // so we poll for a short time.
            for _ in 0..80 {
                match docker.inspect_exec(&exec_id).await {
                    Ok(info) => {
                        if info.running.unwrap_or(false) {
                            tokio::time::sleep(Duration::from_millis(25)).await;
                            continue;
                        }
                        let code = info.exit_code.unwrap_or(0);
                        return if code < 0 { 255 } else { code as u32 };
                    }
                    Err(e) => {
                        warn!("Failed to inspect exec {}: {}", exec_id, e);
                        break;
                    }
                }
            }
            255
        }
        .boxed();

        Ok(ExecProcess {
            output,
            input,
            exit_status,
            resizer,
        })
    }

    /// Current status of a container (by name or id).
    pub async fn status(&self, container_name: &str) -> SandboxStatus {
        match self
            .docker
            .inspect_container(container_name, None::<InspectContainerOptions>)
            .await
        {
            Ok(info) => {
                let state = info.state.as_ref();
                let running = state.and_then(|s| s.running).unwrap_or(false);
                let paused = state.and_then(|s| s.paused).unwrap_or(false);
                let status = if paused {
                    "paused".to_string()
                } else if running {
                    "running".to_string()
                } else {
                    state
                        .and_then(|s| s.status.as_ref().map(|s| s.to_string()))
                        .unwrap_or_else(|| "stopped".to_string())
                };

                let id_short = info
                    .id
                    .as_deref()
                    .map(|id| id.get(..12).unwrap_or(id).to_string());
                SandboxStatus {
                    status,
                    id_short,
                    running,
                    paused,
                }
            }
            Err(BollardError::DockerResponseServerError {
                status_code: 404, ..
            }) => SandboxStatus::missing(),
            Err(_e) => SandboxStatus::error(),
        }
    }

    /// Stop a container (10s grace period).
    pub async fn stop(&self, container_name: &str) -> Result<()> {
        self.docker
            .stop_container(
                container_name,
                Some(StopContainerOptionsBuilder::new().t(10).build()),
            )
            .await
            .with_context(|| format!("stop container {container_name}"))
    }

    /// Pause (freeze) a container.
    pub async fn pause(&self, container_name: &str) -> Result<()> {
        self.docker
            .pause_container(container_name)
            .await
            .with_context(|| format!("pause container {container_name}"))
    }

    /// Resource usage of a running container.
    pub async fn stats(&self, container_name: &str, fast: bool) -> Option<SandboxStats> {
        if fast {
            container_stats_fast(&self.docker, container_name).await
        } else {
            container_stats(&self.docker, container_name).await
        }
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

        // Remove the inner Docker data volume along with the workspace data. It is looked up by
        // name regardless of the current mode, so switching modes never strands a volume.
        let mut removed_volumes = Vec::new();
        if !opts.keep_workspace {
            let volume = inner_docker_volume_name(github_user, project);
            if opts.dry_run {
                if self.docker.inspect_volume(&volume).await.is_ok() {
                    removed_volumes.push(format!("{volume} (dry-run)"));
                }
            } else {
                let rm_opts = RemoveVolumeOptionsBuilder::new().force(opts.force).build();
                match self.docker.remove_volume(&volume, Some(rm_opts)).await {
                    Ok(_) => removed_volumes.push(volume),
                    Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 404, ..
                    }) => {}
                    Err(e) => warnings.push(format!("remove volume {volume}: {e}")),
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
            removed_volumes,
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

/// Resize an exec TTY.
pub async fn resize_exec(docker: &Docker, exec_id: &str, cols: u16, rows: u16) -> Result<()> {
    docker
        .resize_exec(
            exec_id,
            ResizeExecOptions {
                width: cols,
                height: rows,
            },
        )
        .await
        .context("Failed to resize exec")
}

async fn container_stats(docker: &Docker, container_name: &str) -> Option<SandboxStats> {
    let mut stream = docker.stats(
        container_name,
        // Equivalent to `docker stats --no-stream`: the daemon waits for two samples so it can
        // populate `precpu_stats` for CPU% calculation.
        Some(StatsOptionsBuilder::new().stream(false).build()),
    );

    // The daemon may wait for two cycles before returning the single response.
    let next = timeout(Duration::from_secs(5), stream.next()).await.ok()??;
    let stats = next.ok()?;

    let cpu_stats = stats.cpu_stats.as_ref()?;
    let precpu_stats = stats.precpu_stats.as_ref()?;
    let cpu_usage = cpu_stats.cpu_usage.as_ref()?;
    let precpu_usage = precpu_stats.cpu_usage.as_ref()?;

    let cpu_total = cpu_usage.total_usage.unwrap_or(0);
    let cpu_total_pre = precpu_usage.total_usage.unwrap_or(0);
    let system = cpu_stats.system_cpu_usage.unwrap_or(0);
    let system_pre = precpu_stats.system_cpu_usage.unwrap_or(0);

    let cpu_delta = cpu_total.saturating_sub(cpu_total_pre);
    let system_delta = system.saturating_sub(system_pre);

    let percpu_count = cpu_usage
        .percpu_usage
        .as_ref()
        .map(|v| v.len() as u64)
        .unwrap_or(1);
    let online_cpus = cpu_stats
        .online_cpus
        .map(|n| n as u64)
        .filter(|&n| n > 0)
        .unwrap_or(percpu_count.max(1));

    // Prefer Docker's standard calculation when system CPU usage deltas are available.
    // Some engines / cgroup modes can omit or zero out system usage, so fall back to using the
    // daemon-provided timestamps.
    let cpu_percent = if cpu_delta == 0 {
        0.0
    } else if system_delta > 0 {
        (cpu_delta as f64 / system_delta as f64) * online_cpus as f64 * 100.0
    } else {
        match (stats.read.as_deref(), stats.preread.as_deref()) {
            (Some(read), Some(preread)) => {
                let read_ns = DateTime::parse_from_rfc3339(read)
                    .ok()
                    .and_then(|dt| dt.timestamp_nanos_opt());
                let preread_ns = DateTime::parse_from_rfc3339(preread)
                    .ok()
                    .and_then(|dt| dt.timestamp_nanos_opt());
                match (read_ns, preread_ns) {
                    (Some(r), Some(p)) if r > p => {
                        let dt_ns = (r - p) as f64;
                        (cpu_delta as f64 / dt_ns) * 100.0
                    }
                    _ => 0.0,
                }
            }
            _ => 0.0,
        }
    };

    let mem = stats.memory_stats.as_ref().and_then(|m| match (m.usage, m.limit) {
        (Some(u), Some(l)) if l > 0 => Some((u, l)),
        _ => None,
    });

    Some(SandboxStats {
        cpu_percent: Some(cpu_percent),
        mem,
    })
}

/// Fast version for watch mode: uses one_shot for quicker response.
/// CPU% may be less accurate but memory is reliable.
async fn container_stats_fast(docker: &Docker, container_name: &str) -> Option<SandboxStats> {
    let mut stream = docker.stats(
        container_name,
        // one_shot gives a quick single sample (precpu_stats may be empty).
        Some(StatsOptionsBuilder::new().stream(false).one_shot(true).build()),
    );

    let next = timeout(Duration::from_millis(1500), stream.next()).await.ok()??;
    let stats = next.ok()?;

    // Memory is always reliable.
    let mem = stats.memory_stats.as_ref().and_then(|m| match (m.usage, m.limit) {
        (Some(u), Some(l)) if l > 0 => Some((u, l)),
        _ => None,
    });

    // CPU with one_shot: precpu_stats may be empty, so we try but may return None.
    let cpu = (|| {
        let cpu_stats = stats.cpu_stats.as_ref()?;
        let precpu_stats = stats.precpu_stats.as_ref()?;
        let cpu_usage = cpu_stats.cpu_usage.as_ref()?;
        let precpu_usage = precpu_stats.cpu_usage.as_ref()?;

        let cpu_total = cpu_usage.total_usage.unwrap_or(0);
        let cpu_total_pre = precpu_usage.total_usage.unwrap_or(0);
        if cpu_total == 0 || cpu_total_pre == 0 || cpu_total <= cpu_total_pre {
            return None;
        }

        let system = cpu_stats.system_cpu_usage.unwrap_or(0);
        let system_pre = precpu_stats.system_cpu_usage.unwrap_or(0);
        let cpu_delta = cpu_total.saturating_sub(cpu_total_pre);
        let system_delta = system.saturating_sub(system_pre);

        let percpu_count = cpu_usage.percpu_usage.as_ref().map(|v| v.len() as u64).unwrap_or(1);
        let online_cpus = cpu_stats.online_cpus.map(|n| n as u64).filter(|&n| n > 0).unwrap_or(percpu_count.max(1));

        if system_delta > 0 {
            Some((cpu_delta as f64 / system_delta as f64) * online_cpus as f64 * 100.0)
        } else {
            None
        }
    })();

    Some(SandboxStats {
        cpu_percent: cpu,
        mem,
    })
}

/// Build the HostConfig with security settings and mounts.
fn build_host_config(
    config: &GatewayConfig,
    inner_docker: &InnerDocker,
    workspace_path: &Path,
    data_volume: Option<&str>,
) -> Result<HostConfig> {
    let security = &config.container_security;

    let mut host_config = HostConfig {
        // Bind mount the workspace
        binds: Some(vec![format!(
            "{}:/workspace",
            workspace_path.display()
        )]),

        // Add host.docker.internal for reverse port forwarding
        extra_hosts: Some(vec!["host.docker.internal:host-gateway".to_string()]),

        // Never run privileged unless Docker-in-sandbox explicitly asks for it
        privileged: Some(*inner_docker == InnerDocker::Privileged),

        // No access to Docker socket
        // (binds is already set, so docker.sock won't be mounted)

        // Network settings
        network_mode: Some("bridge".to_string()),

        // Init process for proper signal handling
        init: Some(true),

        ..Default::default()
    };

    if let Some(volume) = data_volume {
        host_config
            .binds
            .get_or_insert_with(Vec::new)
            .push(format!("{volume}:/var/lib/docker"));
    }

    match inner_docker {
        InnerDocker::Off | InnerDocker::Privileged => {}
        InnerDocker::Sysbox { runtime } => host_config.runtime = Some(runtime.clone()),
    }

    // Apply security settings. The inner dockerd needs the full capability set; under sysbox
    // those capabilities are confined to the container's user namespace.
    if security.cap_drop_all && *inner_docker == InnerDocker::Off {
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

/// Parse a memory limit string (e.g., "4g", "512m") to bytes.
pub fn parse_memory_limit(s: &str) -> Result<i64> {
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

    fn host_config_for(inner_docker: &InnerDocker, data_volume: Option<&str>) -> HostConfig {
        build_host_config(
            &GatewayConfig::default(),
            inner_docker,
            Path::new("/ws"),
            data_volume,
        )
        .unwrap()
    }

    #[test]
    fn test_resolve_inner_docker() {
        let with_sysbox = |r: &str| r == "sysbox-runc";
        let without = |_: &str| false;
        let sysbox = InnerDocker::Sysbox {
            runtime: "sysbox-runc".to_string(),
        };

        use DockerInSandboxMode::{Auto, Off, On};
        use DockerIsolation::{Privileged, Sysbox};
        let resolve = |mode, isolation, has: &dyn Fn(&str) -> bool| {
            InnerDocker::resolve(mode, isolation, "sysbox-runc", has)
        };
        assert_eq!(resolve(Auto, Sysbox, &with_sysbox).unwrap(), sysbox);
        assert_eq!(resolve(Auto, Sysbox, &without).unwrap(), InnerDocker::Off);
        assert_eq!(resolve(Auto, Privileged, &without).unwrap(), InnerDocker::Off);
        assert_eq!(resolve(On, Sysbox, &with_sysbox).unwrap(), sysbox);
        assert!(resolve(On, Sysbox, &without).is_err());
        assert_eq!(resolve(On, Privileged, &without).unwrap(), InnerDocker::Privileged);
        assert_eq!(resolve(Off, Sysbox, &with_sysbox).unwrap(), InnerDocker::Off);
    }

    #[test]
    fn test_host_config_without_inner_docker_is_locked_down() {
        let hc = host_config_for(&InnerDocker::Off, None);
        assert_eq!(hc.privileged, Some(false));
        assert_eq!(hc.runtime, None);
        assert_eq!(hc.cap_drop, Some(vec!["ALL".to_string()]));
        assert_eq!(hc.binds, Some(vec!["/ws:/workspace".to_string()]));
    }

    #[test]
    fn test_host_config_sysbox() {
        let inner = InnerDocker::Sysbox {
            runtime: "sysbox-runc".to_string(),
        };
        let hc = host_config_for(&inner, Some("agentman-docker-octocat-demo"));
        assert_eq!(hc.privileged, Some(false));
        assert_eq!(hc.runtime.as_deref(), Some("sysbox-runc"));
        assert_eq!(hc.cap_drop, None);
        assert_eq!(
            hc.binds,
            Some(vec![
                "/ws:/workspace".to_string(),
                "agentman-docker-octocat-demo:/var/lib/docker".to_string(),
            ])
        );
    }

    #[test]
    fn test_host_config_privileged() {
        let hc = host_config_for(&InnerDocker::Privileged, None);
        assert_eq!(hc.privileged, Some(true));
        assert_eq!(hc.runtime, None);
        assert_eq!(hc.cap_drop, None);
        assert_eq!(hc.binds, Some(vec!["/ws:/workspace".to_string()]));
    }

    #[test]
    fn test_parse_memory_limit() {
        assert_eq!(parse_memory_limit("4g").unwrap(), 4 * 1024 * 1024 * 1024);
        assert_eq!(parse_memory_limit("512m").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory_limit("1024k").unwrap(), 1024 * 1024);
        assert_eq!(parse_memory_limit("1000").unwrap(), 1000);
        assert_eq!(parse_memory_limit("2G").unwrap(), 2 * 1024 * 1024 * 1024);
    }
}
