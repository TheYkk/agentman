//! Gateway control commands.
//!
//! These are handled by the gateway itself (not inside the container) and are intended to be a
//! small, stable control surface for lifecycle operations like destroying a workspace.

use crate::runtime::{DestroyOptions, Runtime, SandboxStats};
use crate::state::WorkspaceInfo;
use futures::future::join_all;
use std::path::Path;
use tokio::process::Command;
use tokio::time::Duration;

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
    ExecStats { current: bool, watch: bool },
}

#[derive(Debug)]
pub(crate) enum GatewayControlExecution {
    Immediate { exit_status: u32, output: String },
    WatchStats { current: bool, interval: Duration },
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
            let mut watch = false;
            for arg in it {
                match arg {
                    "--current" | "--curennt" => current = true,
                    "--watch" | "-w" => watch = true,
                    "--help" | "-h" => return Some(GatewayControlCommand::Help),
                    _ => return Some(GatewayControlCommand::Help),
                }
            }
            Some(GatewayControlCommand::ExecStats { current, watch })
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
                    let mut watch = false;
                    for arg in it {
                        match arg {
                            "--current" | "--curennt" => current = true,
                            "--watch" | "-w" => watch = true,
                            "--help" | "-h" => return Some(GatewayControlCommand::Help),
                            _ => return Some(GatewayControlCommand::Help),
                        }
                    }
                    Some(GatewayControlCommand::ExecStats { current, watch })
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
  agentman stats [--current] [--watch]

Notes:
  - Without --yes, destroy refuses to delete your persistent workspace directory.
  - --keep-workspace stops/removes container(s) but keeps your files on disk.
  - --dry-run prints what would be deleted.
  - stop/pause apply to the *current* sandbox (the project in your SSH user).
  - stats without --current shows all sandboxes for your GitHub user.
  - --watch refreshes output every second (use Ctrl-C to exit).
  - `agentman exec <cmd>` is accepted as an alias for these commands.
"
    .to_string()
}

pub(crate) async fn execute_gateway_control_command(
    ctrl: GatewayControlCommand,
    runtime: &Runtime,
    github_user: &str,
    project: &str,
) -> GatewayControlExecution {
    match ctrl {
        GatewayControlCommand::Help => GatewayControlExecution::Immediate {
            exit_status: 0u32,
            output: gateway_control_help_text(),
        },
        GatewayControlCommand::Destroy {
            yes,
            keep_workspace,
            dry_run,
            force,
        } => {
            if !dry_run && !keep_workspace && !yes {
                GatewayControlExecution::Immediate {
                    exit_status: 2u32,
                    output: destroy_confirmation_required_text(),
                }
            } else {
                let opts = DestroyOptions {
                    keep_workspace,
                    force,
                    dry_run,
                };

                match runtime.destroy_workspace(github_user, project, opts).await {
                    Ok(res) => GatewayControlExecution::Immediate {
                        exit_status: 0u32,
                        output: res.format_human(),
                    },
                    Err(e) => GatewayControlExecution::Immediate {
                        exit_status: 1u32,
                        output: format!("Destroy failed: {e}\n"),
                    },
                }
            }
        }
        GatewayControlCommand::ExecList => {
            let mut workspaces = runtime.list_workspaces(github_user).await;
            workspaces.sort_by(|a, b| a.project.cmp(&b.project));

            if workspaces.is_empty() {
                return GatewayControlExecution::Immediate {
                    exit_status: 0u32,
                    output: format!("agentman: no sandboxes for {github_user}\n"),
                };
            }

            let mut out = format!("agentman: sandboxes for {github_user}\n");
            for ws in workspaces {
                let is_current = ws.project == project;
                let st = runtime.status(&ws).await;
                let id_suffix = st
                    .id_short
                    .as_deref()
                    .map(|id| format!(" id={id}"))
                    .unwrap_or_default();

                out.push_str(&format!(
                    "- {}{}: {}  container={}{}\n",
                    ws.project,
                    if is_current { " (current)" } else { "" },
                    st.status,
                    ws.container_name,
                    id_suffix
                ));
            }
            GatewayControlExecution::Immediate {
                exit_status: 0u32,
                output: out,
            }
        }
        GatewayControlCommand::ExecStop => {
            let (exit_status, output) = match runtime.get_workspace(github_user, project).await {
                None => (
                    1u32,
                    format!("agentman: no sandbox found for {github_user}/{project}\n"),
                ),
                Some(ws) => {
                    let st = runtime.status(&ws).await;
                    if !st.exists() {
                        (1u32, container_not_found_text(github_user, project, &ws))
                    } else if !st.running {
                        (0u32, format!("agentman: sandbox {project} is already stopped\n"))
                    } else {
                        match runtime.stop(&ws).await {
                            Ok(()) => (
                                0u32,
                                format!(
                                    "agentman: stopped sandbox {project} ({})\n",
                                    ws.container_name
                                ),
                            ),
                            Err(e) => (1u32, format!("agentman: stop failed: {e:#}\n")),
                        }
                    }
                }
            };
            GatewayControlExecution::Immediate { exit_status, output }
        }
        GatewayControlCommand::ExecPause => {
            let (exit_status, output) = match runtime.get_workspace(github_user, project).await {
                None => (
                    1u32,
                    format!("agentman: no sandbox found for {github_user}/{project}\n"),
                ),
                Some(ws) => {
                    let st = runtime.status(&ws).await;
                    if !st.exists() {
                        (1u32, container_not_found_text(github_user, project, &ws))
                    } else if !st.running {
                        (
                            1u32,
                            format!("agentman: sandbox {project} is not running (cannot pause)\n"),
                        )
                    } else if st.paused {
                        (0u32, format!("agentman: sandbox {project} is already paused\n"))
                    } else {
                        match runtime.pause(&ws).await {
                            Ok(()) => (
                                0u32,
                                format!(
                                    "agentman: paused sandbox {project} ({})\n",
                                    ws.container_name
                                ),
                            ),
                            Err(e) => (1u32, format!("agentman: pause failed: {e:#}\n")),
                        }
                    }
                }
            };
            GatewayControlExecution::Immediate { exit_status, output }
        }
        GatewayControlCommand::ExecStats { current, watch } => {
            if watch {
                GatewayControlExecution::WatchStats {
                    current,
                    interval: Duration::from_secs(1),
                }
            } else {
                let (exit_status, output) =
                    render_sandbox_stats(runtime, github_user, project, current).await;
                GatewayControlExecution::Immediate { exit_status, output }
            }
        }
    }
}

fn container_not_found_text(github_user: &str, project: &str, ws: &WorkspaceInfo) -> String {
    format!(
        "agentman: container not found for {github_user}/{project} (expected name {})\n",
        ws.container_name
    )
}

async fn stats_workspaces(
    runtime: &Runtime,
    github_user: &str,
    project: &str,
    current: bool,
) -> Result<Vec<WorkspaceInfo>, (u32, String)> {
    let mut workspaces = if current {
        match runtime.get_workspace(github_user, project).await {
            Some(ws) => vec![ws],
            None => {
                return Err((
                    1u32,
                    format!("agentman: no current sandbox found for {github_user}/{project}\n"),
                ));
            }
        }
    } else {
        runtime.list_workspaces(github_user).await
    };
    workspaces.sort_by(|a, b| a.project.cmp(&b.project));

    if workspaces.is_empty() {
        return Err((0u32, format!("agentman: no sandboxes for {github_user}\n")));
    }
    Ok(workspaces)
}

fn format_stats_fields(id_short: Option<&str>, stats: &SandboxStats) -> String {
    format!(
        "{}{}{}",
        if let Some(id) = id_short {
            format!(" id={id}")
        } else {
            "".to_string()
        },
        if let Some(cpu) = stats.cpu_percent {
            format!(" cpu={:.1}%", cpu)
        } else {
            " cpu=n/a".to_string()
        },
        if let Some((usage, limit)) = stats.mem {
            format!(" mem={}/{}", format_bytes(usage), format_bytes(limit))
        } else {
            " mem=n/a".to_string()
        },
    )
}

pub(crate) async fn render_sandbox_stats(
    runtime: &Runtime,
    github_user: &str,
    project: &str,
    current: bool,
) -> (u32, String) {
    let workspaces = match stats_workspaces(runtime, github_user, project, current).await {
        Ok(ws) => ws,
        Err(res) => return res,
    };

    let mut out = format!("agentman: sandbox stats for {github_user}\n");
    for ws in workspaces {
        let is_current = ws.project == project;
        let st = runtime.status(&ws).await;

        let stats = if st.running {
            runtime.stats(&ws, false).await.unwrap_or_default()
        } else {
            SandboxStats::default()
        };

        let storage = du_bytes(&ws.host_workspace_path).await;

        out.push_str(&format!(
            "- {}{}: status={}{} storage(workspace)={}\n",
            ws.project,
            if is_current { " (current)" } else { "" },
            st.status,
            format_stats_fields(st.id_short.as_deref(), &stats),
            storage
                .map(format_bytes)
                .unwrap_or_else(|| "n/a".to_string())
        ));
    }
    (0u32, out)
}

/// Fast version for watch mode: skips storage (du) and parallelizes stats queries.
pub(crate) async fn render_sandbox_stats_fast(
    runtime: &Runtime,
    github_user: &str,
    project: &str,
    current: bool,
) -> (u32, String) {
    let workspaces = match stats_workspaces(runtime, github_user, project, current).await {
        Ok(ws) => ws,
        Err(res) => return res,
    };

    // Parallelize: gather status + stats for all workspaces at once.
    let futs: Vec<_> = workspaces
        .iter()
        .map(|ws| async move {
            let st = runtime.status(ws).await;
            let stats = if st.running {
                runtime.stats(ws, true).await.unwrap_or_default()
            } else {
                SandboxStats::default()
            };
            (st, stats)
        })
        .collect();

    let results = join_all(futs).await;

    let mut out = format!("agentman: sandbox stats for {github_user}\n");
    for (ws, (st, stats)) in workspaces.iter().zip(results.into_iter()) {
        let is_current = ws.project == project;
        out.push_str(&format!(
            "- {}{}: status={}{}\n",
            ws.project,
            if is_current { " (current)" } else { "" },
            st.status,
            format_stats_fields(st.id_short.as_deref(), &stats),
        ));
    }
    (0u32, out)
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
