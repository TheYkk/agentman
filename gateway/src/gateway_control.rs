//! Gateway control commands.
//!
//! These are handled by the gateway itself (not inside the container) and are intended to be a
//! small, stable control surface for lifecycle operations like destroying a workspace.

use crate::docker::{ContainerManager, DestroyOptions};

#[derive(Debug, Clone, Copy)]
pub(crate) enum GatewayControlCommand {
    Help,
    Destroy {
        yes: bool,
        keep_workspace: bool,
        dry_run: bool,
        force: bool,
    },
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

Notes:
  - Without --yes, destroy refuses to delete your persistent workspace directory.
  - --keep-workspace stops/removes container(s) but keeps your files on disk.
  - --dry-run prints what would be deleted.
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
