//! Runtime-agnostic sandbox interface.
//!
//! The SSH layer talks to a [`Runtime`], which dispatches to either the Docker backend
//! ([`ContainerManager`]) or the Proxmox LXC backend ([`ProxmoxManager`]).

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use tokio::io::AsyncWrite;

use crate::config::RuntimeKind;
use crate::docker::ContainerManager;
use crate::proxmox::ProxmoxManager;
use crate::pty::PtyMaster;
use crate::state::WorkspaceInfo;

/// One async lock per key, used to serialize sandbox creation per workspace so a second
/// connection waits for an in-flight create instead of racing it.
#[derive(Default)]
pub struct KeyedLocks(Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>);

impl KeyedLocks {
    pub async fn lock(&self, key: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.to_string())
            .or_default()
            .clone();
        lock.lock_owned().await
    }
}

/// Receives one-line status updates while a sandbox is being created or started, so the SSH
/// layer can show them to the user instead of sitting silent through a slow first connect.
pub type Progress<'a> = &'a (dyn Fn(&str) + Send + Sync);

/// A [`Progress`] that drops every update.
pub fn no_progress(_: &str) {}

/// Status line written before creating a new sandbox.
pub fn creating_message(project: &str) -> String {
    format!("agentman: creating sandbox {project} (first connect, this can take a minute)...")
}

/// Status line written before starting a stopped sandbox.
pub fn starting_message(project: &str) -> String {
    format!("agentman: starting sandbox {project}...")
}

/// Status line written before resuming a paused sandbox.
pub fn resuming_message(project: &str) -> String {
    format!("agentman: resuming sandbox {project}...")
}

/// Run `fut` in its own task and wait for it. Dropping the returned future (an aborted caller)
/// detaches the task instead of cancelling `fut`.
pub async fn run_detached<T: Send + 'static>(
    fut: impl std::future::Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    tokio::spawn(fut)
        .await
        .map_err(|e| anyhow!("sandbox provisioning task failed: {e}"))?
}

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
    pub removed_volumes: Vec<String>,
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
            "- removed volumes: {}\n",
            if self.removed_volumes.is_empty() {
                "(none)".to_string()
            } else {
                self.removed_volumes.join(", ")
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

/// A chunk of output from a process running in a sandbox.
#[derive(Debug)]
pub enum ExecOutput {
    Stdout(Bytes),
    Stderr(Bytes),
}

/// What to run inside a sandbox.
#[derive(Debug, Clone)]
pub struct ExecSpec {
    pub cmd: Vec<String>,
    pub tty: bool,
    pub env: Vec<String>,
    /// Initial terminal size (cols, rows) when `tty` is set.
    pub size: Option<(u16, u16)>,
}

/// A running process inside a sandbox, with its I/O attached.
pub struct ExecProcess {
    /// Combined output stream; ends when the process exits.
    pub output: BoxStream<'static, std::io::Result<ExecOutput>>,
    /// Process stdin. Dropping it closes stdin.
    pub input: Pin<Box<dyn AsyncWrite + Send>>,
    /// Resolves to the exit status. Await only after `output` has ended.
    pub exit_status: BoxFuture<'static, u32>,
    /// Resizes the process TTY.
    pub resizer: Resizer,
}

/// Handle for resizing a process TTY.
#[derive(Clone)]
pub enum Resizer {
    Docker {
        docker: bollard::Docker,
        exec_id: String,
    },
    Pty(Arc<PtyMaster>),
    None,
}

impl Resizer {
    pub async fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        match self {
            Self::Docker { docker, exec_id } => {
                crate::docker::resize_exec(docker, exec_id, cols, rows).await
            }
            Self::Pty(master) => master.resize(cols, rows),
            Self::None => Ok(()),
        }
    }
}

/// Point-in-time status of a sandbox.
#[derive(Debug, Clone)]
pub struct SandboxStatus {
    /// Human-readable status: running, paused, stopped, missing, error, ...
    pub status: String,
    /// Short id for display (Docker short id, or LXC VMID).
    pub id_short: Option<String>,
    pub running: bool,
    pub paused: bool,
}

impl SandboxStatus {
    pub fn missing() -> Self {
        Self {
            status: "missing".to_string(),
            id_short: None,
            running: false,
            paused: false,
        }
    }

    pub fn error() -> Self {
        Self {
            status: "error".to_string(),
            id_short: None,
            running: false,
            paused: false,
        }
    }

    pub fn exists(&self) -> bool {
        self.status != "missing" && self.status != "error"
    }
}

/// CPU (percent of one core) and memory (usage, limit) of a running sandbox.
#[derive(Debug, Clone, Default)]
pub struct SandboxStats {
    pub cpu_percent: Option<f64>,
    pub mem: Option<(u64, u64)>,
}

/// The sandbox runtime selected by configuration.
pub enum Runtime {
    Docker(ContainerManager),
    Proxmox(ProxmoxManager),
}

impl Runtime {
    pub fn kind(&self) -> RuntimeKind {
        match self {
            Self::Docker(_) => RuntimeKind::Docker,
            Self::Proxmox(_) => RuntimeKind::Proxmox,
        }
    }

    /// Start background work: the Proxmox warm pool and template upkeep.
    pub fn spawn_background(self: &Arc<Self>) {
        if let Self::Proxmox(_) = **self {
            let rt = self.clone();
            tokio::spawn(async move {
                if let Self::Proxmox(m) = &*rt {
                    m.run_pool_loop().await;
                }
            });
        }
    }

    /// Get or create the sandbox for (github_user, project). Returns its id.
    ///
    /// `progress` is told before any slow step (creating, starting or resuming the sandbox).
    pub async fn get_or_create_sandbox(
        &self,
        github_user: &str,
        project: &str,
        progress: Progress<'_>,
    ) -> Result<String> {
        match self {
            Self::Docker(m) => m.get_or_create_container(github_user, project, progress).await,
            Self::Proxmox(m) => m.get_or_create_container(github_user, project, progress).await,
        }
    }

    /// [`Self::get_or_create_sandbox`] in its own task, so it runs to completion even when the
    /// caller is cancelled (the client closes the channel or disconnects mid-create). Otherwise
    /// a sandbox could be claimed or created and started but never recorded in the state file,
    /// leaving a container that `destroy` cannot find.
    pub async fn ensure_sandbox(
        self: &Arc<Self>,
        github_user: &str,
        project: &str,
        progress: impl Fn(&str) + Send + Sync + 'static,
    ) -> Result<String> {
        let rt = self.clone();
        let github_user = github_user.to_string();
        let project = project.to_string();
        run_detached(async move {
            rt.get_or_create_sandbox(&github_user, &project, &progress)
                .await
        })
        .await
    }

    /// Start a process inside a sandbox (by id) with its I/O attached.
    pub async fn exec(&self, sandbox_id: &str, spec: ExecSpec) -> Result<ExecProcess> {
        match self {
            Self::Docker(m) => m.exec(sandbox_id, spec).await,
            Self::Proxmox(m) => m.exec(sandbox_id, spec).await,
        }
    }

    /// List all workspaces for a given GitHub user.
    pub async fn list_workspaces(&self, github_user: &str) -> Vec<WorkspaceInfo> {
        match self {
            Self::Docker(m) => m.list_workspaces(github_user).await,
            Self::Proxmox(m) => m.list_workspaces(github_user).await,
        }
    }

    /// Get workspace info by (github_user, project).
    pub async fn get_workspace(&self, github_user: &str, project: &str) -> Option<WorkspaceInfo> {
        match self {
            Self::Docker(m) => m.get_workspace(github_user, project).await,
            Self::Proxmox(m) => m.get_workspace(github_user, project).await,
        }
    }

    pub async fn destroy_workspace(
        &self,
        github_user: &str,
        project: &str,
        opts: DestroyOptions,
    ) -> Result<DestroyResult> {
        match self {
            Self::Docker(m) => m.destroy_workspace(github_user, project, opts).await,
            Self::Proxmox(m) => m.destroy_workspace(github_user, project, opts).await,
        }
    }

    pub async fn status(&self, ws: &WorkspaceInfo) -> SandboxStatus {
        if ws.runtime != self.kind() {
            return SandboxStatus {
                status: format!("other-runtime({})", ws.runtime),
                id_short: None,
                running: false,
                paused: false,
            };
        }
        match self {
            Self::Docker(m) => m.status(&ws.container_name).await,
            Self::Proxmox(m) => m.status(ws).await,
        }
    }

    pub async fn stop(&self, ws: &WorkspaceInfo) -> Result<()> {
        match self {
            Self::Docker(m) => m.stop(&ws.container_name).await,
            Self::Proxmox(m) => m.stop(ws).await,
        }
    }

    pub async fn pause(&self, ws: &WorkspaceInfo) -> Result<()> {
        match self {
            Self::Docker(m) => m.pause(&ws.container_name).await,
            Self::Proxmox(m) => m.pause(ws).await,
        }
    }

    /// Resource usage of a running sandbox. `fast` trades CPU accuracy for latency.
    pub async fn stats(&self, ws: &WorkspaceInfo, fast: bool) -> Option<SandboxStats> {
        match self {
            Self::Docker(m) => m.stats(&ws.container_name, fast).await,
            Self::Proxmox(m) => m.stats(ws).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn detached_work_survives_an_aborted_caller() {
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let caller = tokio::spawn(run_detached(async move {
            // Like claiming a warm container, starting it and then writing the state file.
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = done_tx.send(());
            Ok(())
        }));
        tokio::time::sleep(Duration::from_millis(10)).await;
        // The SSH channel closes mid-create: the session aborts its I/O task.
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(2), done_rx)
            .await
            .expect("provisioning was cancelled with its caller")
            .unwrap();
    }
}
