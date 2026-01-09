//! Gateway control commands.
//!
//! These are handled by the gateway itself (not inside the container) and are intended to be a
//! small, stable control surface for lifecycle operations like destroying a workspace.

use bollard::errors::Error as BollardError;
use bollard::query_parameters::{
    InspectContainerOptions, StatsOptionsBuilder, StopContainerOptionsBuilder,
};
use crate::docker::{ContainerManager, DestroyOptions};
use futures::StreamExt;
use std::path::Path;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

#[derive(Debug, Clone, Copy)]
pub(crate) enum GatewayControlCommand {
    Help,
    Destroy {
        yes: bool,
        keep_workspace: bool,
        dry_run: bool,
        force: bool,
    },
    ExecList,
    ExecStop,
    ExecPause,
    ExecStats { current: bool },
}

pub(crate) fn parse_gateway_control_command(cmd: &str) -> Option<GatewayControlCommand> {
    let mut it = cmd.split_whitespace();
    let first = it.next()?;
    if first != "agentman" {
        return None;
    }

    let sub = it.next().unwrap_or("help");
    match sub {
        "help" | "--help" | "-h" => Some(GatewayControlCommand::Help),
        "list" => {
            if it.next().is_some() {
                Some(GatewayControlCommand::Help)
            } else {
                Some(GatewayControlCommand::ExecList)
            }
        }
        "stop" => {
            if it.next().is_some() {
                Some(GatewayControlCommand::Help)
            } else {
                Some(GatewayControlCommand::ExecStop)
            }
        }
        "pause" => {
            if it.next().is_some() {
                Some(GatewayControlCommand::Help)
            } else {
                Some(GatewayControlCommand::ExecPause)
            }
        }
        "stats" => {
            let mut current = false;
            for arg in it {
                match arg {
                    "--current" | "--curennt" => current = true,
                    "--help" | "-h" => return Some(GatewayControlCommand::Help),
                    _ => return Some(GatewayControlCommand::Help),
                }
            }
            Some(GatewayControlCommand::ExecStats { current })
        }
        "exec" => {
            let action = it.next().unwrap_or("help");
            match action {
                "help" | "--help" | "-h" => Some(GatewayControlCommand::Help),
                "list" => {
                    if it.next().is_some() {
                        Some(GatewayControlCommand::Help)
                    } else {
                        Some(GatewayControlCommand::ExecList)
                    }
                }
                "stop" => {
                    if it.next().is_some() {
                        Some(GatewayControlCommand::Help)
                    } else {
                        Some(GatewayControlCommand::ExecStop)
                    }
                }
                "pause" => {
                    if it.next().is_some() {
                        Some(GatewayControlCommand::Help)
                    } else {
                        Some(GatewayControlCommand::ExecPause)
                    }
                }
                "stats" => {
                    let mut current = false;
                    for arg in it {
                        match arg {
                            "--current" | "--curennt" => current = true,
                            "--help" | "-h" => return Some(GatewayControlCommand::Help),
                            _ => return Some(GatewayControlCommand::Help),
                        }
                    }
                    Some(GatewayControlCommand::ExecStats { current })
                }
                _ => Some(GatewayControlCommand::Help),
            }
        }
        "destroy" => {
            let mut yes = false;
            let mut keep_workspace = false;
            let mut dry_run = false;
            let mut force = false;

            for arg in it {
                match arg {
                    "--yes" | "-y" => yes = true,
                    "--keep-workspace" => keep_workspace = true,
                    "--dry-run" => dry_run = true,
                    "--force" => force = true,
                    "--help" | "-h" => return Some(GatewayControlCommand::Help),
                    _ => {
                        // Unknown args fall back to help (keeps behavior stable).
                        return Some(GatewayControlCommand::Help);
                    }
                }
            }

            Some(GatewayControlCommand::Destroy {
                yes,
                keep_workspace,
                dry_run,
                force,
            })
        }
        _ => Some(GatewayControlCommand::Help),
    }
}

pub(crate) fn gateway_control_help_text() -> String {
    // Keep this compatible with non-interactive SSH exec flows.
    "\
agentman gateway control commands

Usage:
  agentman destroy [--yes] [--keep-workspace] [--dry-run] [--force]
  agentman list
  agentman stop
  agentman pause
  agentman stats [--current]

Notes:
  - Without --yes, destroy refuses to delete your persistent workspace directory.
  - --keep-workspace stops/removes container(s) but keeps your files on disk.
  - --dry-run prints what would be deleted.
  - stop/pause apply to the *current* sandbox (the project in your SSH user).
  - stats without --current shows all sandboxes for your GitHub user.
  - `agentman exec <cmd>` is accepted as an alias for these commands.
"
    .to_string()
}

pub(crate) async fn execute_gateway_control_command(
    ctrl: GatewayControlCommand,
    container_manager: &ContainerManager,
    github_user: &str,
    project: &str,
) -> (u32, String) {
    match ctrl {
        GatewayControlCommand::Help => (0u32, gateway_control_help_text()),
        GatewayControlCommand::Destroy {
            yes,
            keep_workspace,
            dry_run,
            force,
        } => {
            if !dry_run && !keep_workspace && !yes {
                (2u32, destroy_confirmation_required_text())
            } else {
                let opts = DestroyOptions {
                    keep_workspace,
                    force,
                    dry_run,
                };

                match container_manager
                    .destroy_workspace(github_user, project, opts)
                    .await
                {
                    Ok(res) => (0u32, res.format_human()),
                    Err(e) => (1u32, format!("Destroy failed: {e}\n")),
                }
            }
        }
        GatewayControlCommand::ExecList => {
            let mut workspaces = container_manager.list_workspaces(github_user).await;
            workspaces.sort_by(|a, b| a.project.cmp(&b.project));

            if workspaces.is_empty() {
                return (0u32, format!("agentman: no sandboxes for {github_user}\n"));
            }

            let mut out = format!("agentman: sandboxes for {github_user}\n");
            for ws in workspaces {
                let is_current = ws.project == project;
                let (status, id_short) =
                    workspace_container_status(container_manager, &ws.container_name).await;
                let id_suffix = id_short
                    .as_deref()
                    .map(|id| format!(" id={id}"))
                    .unwrap_or_default();

                out.push_str(&format!(
                    "- {}{}: {}  container={}{}\n",
                    ws.project,
                    if is_current { " (current)" } else { "" },
                    status,
                    ws.container_name,
                    id_suffix
                ));
            }
            (0u32, out)
        }
        GatewayControlCommand::ExecStop => match container_manager.get_workspace(github_user, project).await {
            None => (
                1u32,
                format!("agentman: no sandbox found for {github_user}/{project}\n"),
            ),
            Some(ws) => {
                let docker = container_manager.docker();
                let info = match docker
                    .inspect_container(&ws.container_name, None::<InspectContainerOptions>)
                    .await
                {
                    Ok(info) => info,
                    Err(BollardError::DockerResponseServerError {
                        status_code: 404, ..
                    }) => {
                        return (
                            1u32,
                            format!(
                                "agentman: container not found for {github_user}/{project} (expected name {})\n",
                                ws.container_name
                            ),
                        );
                    }
                    Err(e) => {
                        return (
                            1u32,
                            format!("agentman: failed to inspect container {}: {e}\n", ws.container_name),
                        );
                    }
                };

                let running = info
                    .state
                    .as_ref()
                    .and_then(|s| s.running)
                    .unwrap_or(false);

                if !running {
                    return (
                        0u32,
                        format!("agentman: sandbox {project} is already stopped\n"),
                    );
                }

                match docker
                    .stop_container(
                        &ws.container_name,
                        Some(StopContainerOptionsBuilder::new().t(10).build()),
                    )
                    .await
                {
                    Ok(_) => (
                        0u32,
                        format!("agentman: stopped sandbox {project} ({})\n", ws.container_name),
                    ),
                    Err(BollardError::DockerResponseServerError {
                        status_code: 404, ..
                    }) => (
                        1u32,
                        format!("agentman: container not found: {}\n", ws.container_name),
                    ),
                    Err(e) => (1u32, format!("agentman: stop failed: {e}\n")),
                }
            }
        },
        GatewayControlCommand::ExecPause => match container_manager.get_workspace(github_user, project).await {
            None => (
                1u32,
                format!("agentman: no sandbox found for {github_user}/{project}\n"),
            ),
            Some(ws) => {
                let docker = container_manager.docker();
                let info = match docker
                    .inspect_container(&ws.container_name, None::<InspectContainerOptions>)
                    .await
                {
                    Ok(info) => info,
                    Err(BollardError::DockerResponseServerError {
                        status_code: 404, ..
                    }) => {
                        return (
                            1u32,
                            format!(
                                "agentman: container not found for {github_user}/{project} (expected name {})\n",
                                ws.container_name
                            ),
                        );
                    }
                    Err(e) => {
                        return (
                            1u32,
                            format!("agentman: failed to inspect container {}: {e}\n", ws.container_name),
                        );
                    }
                };

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

                if !running {
                    return (
                        1u32,
                        format!("agentman: sandbox {project} is not running (cannot pause)\n"),
                    );
                }
                if paused {
                    return (
                        0u32,
                        format!("agentman: sandbox {project} is already paused\n"),
                    );
                }

                match docker.pause_container(&ws.container_name).await {
                    Ok(_) => (
                        0u32,
                        format!("agentman: paused sandbox {project} ({})\n", ws.container_name),
                    ),
                    Err(BollardError::DockerResponseServerError {
                        status_code: 404, ..
                    }) => (
                        1u32,
                        format!("agentman: container not found: {}\n", ws.container_name),
                    ),
                    Err(e) => (1u32, format!("agentman: pause failed: {e}\n")),
                }
            }
        },
        GatewayControlCommand::ExecStats { current } => {
            let mut workspaces = if current {
                match container_manager.get_workspace(github_user, project).await {
                    Some(ws) => vec![ws],
                    None => {
                        return (
                            1u32,
                            format!("agentman: no current sandbox found for {github_user}/{project}\n"),
                        );
                    }
                }
            } else {
                container_manager.list_workspaces(github_user).await
            };
            workspaces.sort_by(|a, b| a.project.cmp(&b.project));

            if workspaces.is_empty() {
                return (0u32, format!("agentman: no sandboxes for {github_user}\n"));
            }

            let mut out = format!("agentman: sandbox stats for {github_user}\n");
            for ws in workspaces {
                let is_current = ws.project == project;
                let (status, id_short, running) =
                    workspace_container_status_with_running(container_manager, &ws.container_name)
                        .await;

                let (cpu, mem) = if running {
                    match container_stats_line(container_manager, &ws.container_name).await {
                        Some((cpu, mem)) => (Some(cpu), mem),
                        None => (None, None),
                    }
                } else {
                    (None, None)
                };

                let storage = du_bytes(&ws.host_workspace_path).await;

                out.push_str(&format!(
                    "- {}{}: status={}{}{}{} storage(workspace)={}\n",
                    ws.project,
                    if is_current { " (current)" } else { "" },
                    status,
                    if let Some(id) = id_short.as_deref() {
                        format!(" id={id}")
                    } else {
                        "".to_string()
                    },
                    if let Some(cpu) = cpu {
                        format!(" cpu={:.1}%", cpu)
                    } else {
                        " cpu=n/a".to_string()
                    },
                    if let Some((usage, limit)) = mem {
                        format!(" mem={}/{}", format_bytes(usage), format_bytes(limit))
                    } else {
                        " mem=n/a".to_string()
                    },
                    storage
                        .map(format_bytes)
                        .unwrap_or_else(|| "n/a".to_string())
                ));
            }
            (0u32, out)
        }
    }
}

fn destroy_confirmation_required_text() -> String {
    "Refusing to destroy without confirmation.\n\
This will stop/remove your container(s) and DELETE your persistent workspace.\n\n\
Run one of:\n\
  agentman destroy --yes\n\
  agentman destroy --keep-workspace\n\
  agentman destroy --dry-run\n"
        .to_string()
}

async fn workspace_container_status(
    container_manager: &ContainerManager,
    container_name: &str,
) -> (String, Option<String>) {
    let (status, id, _running) =
        workspace_container_status_with_running(container_manager, container_name).await;
    (status, id)
}

async fn workspace_container_status_with_running(
    container_manager: &ContainerManager,
    container_name: &str,
) -> (String, Option<String>, bool) {
    let docker = container_manager.docker();
    match docker
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
            (status, id_short, running)
        }
        Err(BollardError::DockerResponseServerError {
            status_code: 404, ..
        }) => ("missing".to_string(), None, false),
        Err(_e) => ("error".to_string(), None, false),
    }
}

async fn container_stats_line(
    container_manager: &ContainerManager,
    container_name: &str,
) -> Option<(f64, Option<(u64, u64)>)> {
    let docker = container_manager.docker();
    let mut stream = docker.stats(
        container_name,
        Some(StatsOptionsBuilder::new().stream(false).one_shot(true).build()),
    );

    let next = timeout(Duration::from_secs(2), stream.next()).await.ok()??;
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
        .unwrap_or(percpu_count);

    let cpu_percent = if system_delta > 0 && cpu_delta > 0 {
        (cpu_delta as f64 / system_delta as f64) * online_cpus as f64 * 100.0
    } else {
        0.0
    };

    let mem = stats.memory_stats.as_ref().and_then(|m| match (m.usage, m.limit) {
        (Some(u), Some(l)) if l > 0 => Some((u, l)),
        _ => None,
    });

    Some((cpu_percent, mem))
}

async fn du_bytes(path: &Path) -> Option<u64> {
    let out = Command::new("du")
        .arg("-s")
        .arg("--block-size=1")
        .arg(path)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let first = stdout.split_whitespace().next()?;
    first.parse::<u64>().ok()
}

fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * KB;
    const GB: f64 = 1024.0 * MB;
    const TB: f64 = 1024.0 * GB;

    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < MB {
        format!("{:.1} KiB", b / KB)
    } else if b < GB {
        format!("{:.1} MiB", b / MB)
    } else if b < TB {
        format!("{:.1} GiB", b / GB)
    } else {
        format!("{:.1} TiB", b / TB)
    }
}
