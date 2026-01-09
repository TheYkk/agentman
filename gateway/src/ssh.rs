//! SSH server implementation using russh.
//!
//! Handles:
//! - Public key authentication with GitHub verification
//! - Session channels (shell, exec)
//! - Port forwarding (direct-tcpip, tcpip-forward)

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use bollard::exec::StartExecResults;
use bollard::container::LogOutput;
use chrono::Utc;
use futures::StreamExt;
use russh::server::{Auth, Handler, Msg, Session};
use russh::{Channel, ChannelId, CryptoVec, MethodKind, MethodSet};
use russh::keys::PublicKey;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::{GatewayConfig, ShellMode};
use crate::docker::{ContainerManager, DestroyOptions};
use crate::github::{
    compute_fingerprint_from_pubkey, parse_ssh_username, public_key_to_openssh,
    validate_github_username, validate_project_name, GitHubKeyFetcher,
};
use crate::state::{KeyCacheEntry, StateManager};

/// Shared state for the SSH server.
pub struct ServerState {
    pub config: Arc<GatewayConfig>,
    pub state: Arc<StateManager>,
    pub container_manager: Arc<ContainerManager>,
    pub github_fetcher: Arc<GitHubKeyFetcher>,
}

/// Per-connection handler state.
pub struct ConnectionHandler {
    /// Shared server state.
    server: Arc<ServerState>,

    /// Client's socket address.
    peer_addr: SocketAddr,

    /// Authenticated GitHub username (set after auth).
    github_user: Option<String>,

    /// Project name (parsed from SSH username).
    project: Option<String>,

    /// Container ID (after provisioning).
    container_id: Option<String>,

    /// Active exec sessions (channel_id -> exec_id).
    exec_sessions: HashMap<ChannelId, ExecSession>,

    /// Pending GitHub username for keyboard-interactive auth.
    pending_github_user: Option<String>,

    /// Active remote port forwards (bind_addr -> listener task handle).
    remote_forwards: HashMap<(String, u32), tokio::task::JoinHandle<()>>,

    /// All public key fingerprints offered during this auth session.
    /// We cache all of them once GitHub verification succeeds.
    offered_key_fingerprints: Vec<String>,

    /// PTY info per SSH channel (set by pty_request).
    ptys: HashMap<ChannelId, PtyInfo>,
}

struct ExecSession {
    exec_id: String,
    tty: bool,
    /// Channel for sending data to the container.
    stdin_tx: Option<mpsc::Sender<Vec<u8>>>,
}

#[derive(Debug, Clone)]
struct PtyInfo {
    term: String,
    cols: u32,
    rows: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelStreamKind {
    /// Normal SSH session channels (shell/exec): return exit-status and keep stderr separate.
    Session,
    /// TCP forwarding channels (direct-tcpip / forwarded-tcpip): treat as raw byte streams.
    TcpForward,
}

fn exec_env(tty: bool, term: &str) -> Vec<String> {
    // Keep this small and non-invasive:
    // - Zed (and other editors) probe `$SHELL` over non-PTY exec sessions.
    // - Some clients run `cd; ...` which fails if HOME is missing.
    let mut env = vec!["SHELL=/bin/bash".to_string()];
    if tty {
        env.push(format!("TERM={}", term));
    } else {
        env.push("HOME=/workspace".to_string());
    }
    env
}

impl ConnectionHandler {
    fn new(server: Arc<ServerState>, peer_addr: SocketAddr) -> Self {
        Self {
            server,
            peer_addr,
            github_user: None,
            project: None,
            container_id: None,
            exec_sessions: HashMap::new(),
            pending_github_user: None,
            remote_forwards: HashMap::new(),
            offered_key_fingerprints: Vec::new(),
            ptys: HashMap::new(),
        }
    }
}

impl Handler for ConnectionHandler {
    type Error = anyhow::Error;

    /// Called when a new client connects.
    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        debug!("Session channel opened: {:?}", channel.id());
        Ok(true)
    }

    /// Handle public key authentication.
    async fn auth_publickey_offered(
        &mut self,
        user: &str,
        public_key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        debug!("Public key offered by user '{}' from {}", user, self.peer_addr);

        // Parse username to extract project and optional github user hint
        let (project, github_hint) = parse_ssh_username(user);

        // Validate project name
        if let Err(e) = validate_project_name(&project) {
            warn!("Invalid project name '{}': {}", project, e);
            return Ok(Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            });
        }

        self.project = Some(project.clone());

        // Get key fingerprint
        let fingerprint = compute_fingerprint_from_pubkey(public_key);
        debug!("Key fingerprint: {}", fingerprint);

        // Track all offered keys so we can cache them all once verified
        if !self.offered_key_fingerprints.contains(&fingerprint) {
            self.offered_key_fingerprints.push(fingerprint.clone());
        }

        // Check if we have this key cached
        if let Some(cached) = self.server.state.get_github_user(&fingerprint).await {
            info!(
                "Found cached GitHub user '{}' for key {}",
                cached.github_username, fingerprint
            );
            self.github_user = Some(cached.github_username);
            return Ok(Auth::Accept);
        }


        // Check if we have a pending GitHub user from keyboard-interactive
        // (This happens when user already entered their GitHub username)
        if let Some(ref github_user) = self.pending_github_user {
            debug!("Verifying key against pending GitHub user '{}'", github_user);
            
            let openssh_key = public_key_to_openssh(public_key);

            match self
                .server
                .github_fetcher
                .verify_key(github_user, &openssh_key)
                .await
            {
                Ok(verified_type) => {
                    info!(
                        "Verified key for GitHub user '{}' (type: {})",
                        github_user, verified_type
                    );

                    // Cache ALL offered keys for this GitHub user, not just the verified one
                    self.cache_all_offered_keys(github_user, &verified_type).await;

                    self.github_user = Some(github_user.clone());
                    self.pending_github_user = None;
                    return Ok(Auth::Accept);
                }
                Err(e) => {
                    warn!(
                        "Key did not match GitHub user '{}': {}. Trying other keys.",
                        github_user, e
                    );
                    // Keep publickey enabled so the client can try another key without re-prompting.
                    let methods =
                        MethodSet::from(&[MethodKind::PublicKey, MethodKind::KeyboardInteractive][..]);
                    return Ok(Auth::Reject {
                        proceed_with_methods: Some(methods),
                        partial_success: false,
                    });
                }
            }
        }

        // If github hint provided in SSH username (e.g., "project+githubuser"), verify against GitHub
        if let Some(github_user) = github_hint {
            if let Err(e) = validate_github_username(&github_user) {
                warn!("Invalid GitHub username '{}': {}", github_user, e);
                return Ok(Auth::Reject {
                    proceed_with_methods: None,
                    partial_success: false,
                });
            }

            let openssh_key = public_key_to_openssh(public_key);

            match self
                .server
                .github_fetcher
                .verify_key(&github_user, &openssh_key)
                .await
            {
                Ok(verified_type) => {
                    info!(
                        "Verified key for GitHub user '{}' (type: {})",
                        github_user, verified_type
                    );

                    // Cache ALL offered keys for this GitHub user
                    self.cache_all_offered_keys(&github_user, &verified_type).await;

                    self.github_user = Some(github_user);
                    return Ok(Auth::Accept);
                }
                Err(e) => {
                    warn!("Failed to verify key for '{}': {}", github_user, e);
                    return Ok(Auth::Reject {
                        proceed_with_methods: None,
                        partial_success: false,
                    });
                }
            }
        }

        // Check bootstrap users
        let openssh_key = public_key_to_openssh(public_key);
        for bootstrap_user in &self.server.config.bootstrap_github_users {
            if let Ok(verified_type) = self
                .server
                .github_fetcher
                .verify_key(bootstrap_user, &openssh_key)
                .await
            {
                info!(
                    "Matched key to bootstrap user '{}' (type: {})",
                    bootstrap_user, verified_type
                );

                // Cache ALL offered keys for this GitHub user
                self.cache_all_offered_keys(bootstrap_user, &verified_type).await;

                self.github_user = Some(bootstrap_user.clone());
                return Ok(Auth::Accept);
            }
        }

        // No match found yet. Keep publickey enabled so the client can try other keys.
        // Keyboard-interactive remains enabled as a fallback after keys are exhausted.
        debug!(
            "Key {} not cached for {}, allowing client to try other keys",
            fingerprint, self.peer_addr
        );
        let methods = MethodSet::from(&[MethodKind::PublicKey, MethodKind::KeyboardInteractive][..]);
        Ok(Auth::Reject {
            proceed_with_methods: Some(methods),
            partial_success: false,
        })
    }

    /// Handle keyboard-interactive authentication (for getting GitHub username).
    async fn auth_keyboard_interactive(
        &mut self,
        user: &str,
        _submethods: &str,
        response: Option<russh::server::Response<'_>>,
    ) -> Result<Auth, Self::Error> {
        debug!("Keyboard-interactive auth for user '{}'", user);

        match response {
            None => {
                // Initial request - ask for GitHub username
                Ok(Auth::Partial {
                    name: "GitHub Username".into(),
                    instructions: "Enter your GitHub username to verify your SSH key:".into(),
                    prompts: vec![("GitHub username: ".into(), true)].into(),
                })
            }
            Some(response) => {
                // Got response - verify the GitHub username
                let responses: Vec<String> = response
                    .into_iter()
                    .map(|r| String::from_utf8_lossy(&r).to_string())
                    .collect();
                if responses.is_empty() {
                    return Ok(Auth::Reject {
                        proceed_with_methods: None,
                        partial_success: false,
                    });
                }

                let github_user = responses[0].clone();
                if let Err(e) = validate_github_username(&github_user) {
                    warn!("Invalid GitHub username '{}': {}", github_user, e);
                    return Ok(Auth::Reject {
                        proceed_with_methods: None,
                        partial_success: false,
                    });
                }

                self.pending_github_user = Some(github_user);

                // Now we need to get the public key again via publickey auth
                let methods = MethodSet::from(&[MethodKind::PublicKey][..]);
                Ok(Auth::Reject {
                    proceed_with_methods: Some(methods),
                    partial_success: false,
                })
            }
        }
    }

    /// Handle verified public key authentication (signature received).
    async fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        debug!("Public key auth (with signature) for user '{}'", user);

        // Track this key too
        let fingerprint = compute_fingerprint_from_pubkey(public_key);
        if !self.offered_key_fingerprints.contains(&fingerprint) {
            self.offered_key_fingerprints.push(fingerprint.clone());
        }

        // If we already have a github_user from offered phase, accept
        if self.github_user.is_some() {
            return Ok(Auth::Accept);
        }

        // If we have a pending github user from keyboard-interactive, verify
        if let Some(github_user) = self.pending_github_user.take() {
            let openssh_key = public_key_to_openssh(public_key);

            match self
                .server
                .github_fetcher
                .verify_key(&github_user, &openssh_key)
                .await
            {
                Ok(verified_type) => {
                    // Cache ALL offered keys for this GitHub user
                    self.cache_all_offered_keys(&github_user, &verified_type).await;

                    self.github_user = Some(github_user);
                    return Ok(Auth::Accept);
                }
                Err(e) => {
                    warn!("Failed to verify key: {}", e);
                    return Ok(Auth::Reject {
                        proceed_with_methods: None,
                        partial_success: false,
                    });
                }
            }
        }

        Ok(Auth::Reject {
            proceed_with_methods: None,
            partial_success: false,
        })
    }

    /// Handle PTY request.
    async fn pty_request(
        &mut self,
        channel_id: ChannelId,
        term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!(
            "PTY request: channel={:?}, term={}, cols={}, rows={}",
            channel_id, term, col_width, row_height
        );
        // Store PTY info for use when creating exec
        let term = if term.is_empty() { "xterm-256color" } else { term };
        self.ptys.insert(
            channel_id,
            PtyInfo {
                term: term.to_string(),
                cols: col_width,
                rows: row_height,
            },
        );
        // Client requested a PTY; confirm success.
        session.channel_success(channel_id)?;
        Ok(())
    }

    /// Handle shell request.
    async fn shell_request(
        &mut self,
        channel_id: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        info!("Shell request on channel {:?}", channel_id);

        let github_user = self
            .github_user
            .as_ref()
            .ok_or_else(|| anyhow!("Not authenticated"))?;
        let project = self
            .project
            .as_ref()
            .ok_or_else(|| anyhow!("No project specified"))?;

        // Get or create container
        let container_id = self
            .server
            .container_manager
            .get_or_create_container(github_user, project)
            .await?;

        self.container_id = Some(container_id.clone());

        let (tty, term) = match self.ptys.get(&channel_id) {
            Some(pty) => (true, pty.term.as_str()),
            None => (false, "xterm-256color"),
        };

        let cmd = match self.server.config.shell.mode {
            ShellMode::Bash => vec!["/bin/bash".to_string(), "-l".to_string()],
            ShellMode::Tmux => {
                // Only start tmux when the client requested a PTY (true interactive session).
                // This avoids breaking editor/bootstrap flows that use non-PTY sessions.
                if tty {
                    let session_name =
                        sanitize_tmux_session_name(&self.server.config.shell.tmux_session);
                    let script = format!(
                        "if command -v tmux >/dev/null 2>&1; then exec tmux new-session -A -s '{session}' -c /workspace /bin/bash -l; else exec /bin/bash -l; fi",
                        session = session_name
                    );
                    vec!["/bin/bash".to_string(), "-lc".to_string(), script]
                } else {
                    vec!["/bin/bash".to_string(), "-l".to_string()]
                }
            }
        };

        // Create exec in container
        let exec_id = self
            .server
            .container_manager
            .create_exec(
                &container_id,
                cmd,
                tty,
                Some(exec_env(tty, term)),
            )
            .await?;

        // Start exec and connect to channel
        self.start_exec_session(
            channel_id,
            exec_id.clone(),
            tty,
            ChannelStreamKind::Session,
            session,
        )
            .await?;

        // Confirm the shell request was accepted (client may be waiting on this).
        session.channel_success(channel_id)?;

        // Resize to stored PTY dimensions
        if let Some(pty) = self.ptys.get(&channel_id) {
            if let Err(e) = self
                .server
                .container_manager
                .resize_exec(&exec_id, pty.cols as u16, pty.rows as u16)
                .await
            {
                warn!("Failed to set initial exec size: {}", e);
            }
        }

        Ok(())
    }

    /// Handle exec request.
    async fn exec_request(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).to_string();
        info!("Exec request on channel {:?}: {}", channel_id, command);

        let github_user = self
            .github_user
            .as_ref()
            .ok_or_else(|| anyhow!("Not authenticated"))?;
        let project = self
            .project
            .as_ref()
            .ok_or_else(|| anyhow!("No project specified"))?;

        // Gateway control commands (handled by the gateway itself, not inside the container).
        // This is intentionally a very small "control surface" to keep behavior predictable.
        if let Some(ctrl) = parse_gateway_control_command(command.trim()) {
            let (exit_status, output) = match ctrl {
                GatewayControlCommand::Help => (0u32, gateway_control_help_text()),
                GatewayControlCommand::Destroy {
                    yes,
                    keep_workspace,
                    dry_run,
                    force,
                } => {
                    if !dry_run && !keep_workspace && !yes {
                        (
                            2u32,
                            format!(
                                "Refusing to destroy without confirmation.\n\
This will stop/remove your container(s) and DELETE your persistent workspace.\n\n\
Run one of:\n\
  agentman destroy --yes\n\
  agentman destroy --keep-workspace\n\
  agentman destroy --dry-run\n"
                            ),
                        )
                    } else {
                        let opts = DestroyOptions {
                            keep_workspace,
                            force,
                            dry_run,
                        };

                        match self
                            .server
                            .container_manager
                            .destroy_workspace(github_user, project, opts)
                            .await
                        {
                            Ok(res) => (0u32, res.format_human()),
                            Err(e) => (1u32, format!("Destroy failed: {e}\n")),
                        }
                    }
                }
            };

            // Confirm the exec request was accepted (OpenSSH sets want-reply=true).
            session.channel_success(channel_id)?;

            let handle = session.handle();
            if !output.is_empty() {
                let _ = handle
                    .data(channel_id, CryptoVec::from_slice(output.as_bytes()))
                    .await;
            }
            let _ = handle.exit_status_request(channel_id, exit_status).await;
            let _ = handle.eof(channel_id).await;
            let _ = handle.close(channel_id).await;
            return Ok(());
        }

        // Get or create container
        let container_id = self
            .server
            .container_manager
            .get_or_create_container(github_user, project)
            .await?;

        self.container_id = Some(container_id.clone());

        let (tty, term) = match self.ptys.get(&channel_id) {
            Some(pty) => (true, pty.term.as_str()),
            None => (false, "xterm-256color"),
        };

        // Create exec in container
        let exec_id = self
            .server
            .container_manager
            .create_exec(
                &container_id,
                // Exec requests should behave like standard sshd: don't force a login shell.
                // This avoids user rc files (e.g. tmux auto-attach) breaking editor bootstrap flows.
                vec!["/bin/bash".to_string(), "-c".to_string(), command],
                tty,
                Some(exec_env(tty, term)),
            )
            .await?;

        // Start exec and connect to channel
        self.start_exec_session(
            channel_id,
            exec_id.clone(),
            tty,
            ChannelStreamKind::Session,
            session,
        )
            .await?;

        // Confirm the exec request was accepted (OpenSSH sets want-reply=true).
        session.channel_success(channel_id)?;

        // Resize to stored PTY dimensions
        if let Some(pty) = self.ptys.get(&channel_id) {
            if let Err(e) = self
                .server
                .container_manager
                .resize_exec(&exec_id, pty.cols as u16, pty.rows as u16)
                .await
            {
                warn!("Failed to set initial exec size: {}", e);
            }
        }

        Ok(())
    }

    /// Handle window change request.
    async fn window_change_request(
        &mut self,
        channel_id: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!(
            "Window change: channel={:?}, cols={}, rows={}",
            channel_id, col_width, row_height
        );

        if let Some(pty) = self.ptys.get_mut(&channel_id) {
            pty.cols = col_width;
            pty.rows = row_height;
        }

        if let Some(exec_session) = self.exec_sessions.get(&channel_id) {
            if !exec_session.tty {
                return Ok(());
            }
            if let Err(e) = self
                .server
                .container_manager
                .resize_exec(&exec_session.exec_id, col_width as u16, row_height as u16)
                .await
            {
                warn!("Failed to resize exec: {}", e);
            }
        }

        Ok(())
    }

    /// Handle data from client.
    async fn data(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(exec_session) = self.exec_sessions.get(&channel_id) {
            if let Some(ref tx) = exec_session.stdin_tx {
                let _ = tx.send(data.to_vec()).await;
            }
        }
        Ok(())
    }

    /// Handle channel close.
    async fn channel_close(
        &mut self,
        channel_id: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!("Channel closed: {:?}", channel_id);
        self.exec_sessions.remove(&channel_id);
        self.ptys.remove(&channel_id);
        Ok(())
    }

    /// Handle channel EOF.
    async fn channel_eof(
        &mut self,
        channel_id: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!("Channel EOF: {:?}", channel_id);
        // Drop the stdin sender to signal EOF to container
        if let Some(exec_session) = self.exec_sessions.get_mut(&channel_id) {
            exec_session.stdin_tx = None;
        }
        Ok(())
    }

    /// Handle direct-tcpip (local port forward) request.
    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        originator_address: &str,
        originator_port: u32,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        if !self.server.config.port_forwarding.allow_local {
            warn!("Local port forwarding disabled");
            return Ok(false);
        }

        info!(
            "Direct-tcpip request: {}:{} from {}:{}",
            host_to_connect, port_to_connect, originator_address, originator_port
        );

        // Ensure we have a container for this connection. VS Code Remote-SSH relies heavily on
        // connecting to loopback ports (127.0.0.1) *inside* the remote environment.
        let github_user = self
            .github_user
            .as_ref()
            .ok_or_else(|| anyhow!("Not authenticated"))?;
        let project = self
            .project
            .as_ref()
            .ok_or_else(|| anyhow!("No project specified"))?;

        let container_id = match self.container_id.clone() {
            Some(id) => id,
            None => {
                let id = self
                    .server
                    .container_manager
                    .get_or_create_container(github_user, project)
                    .await?;
                self.container_id = Some(id.clone());
                id
            }
        };

        // Determine destination inside the container.
        // - For localhost requests: always connect to 127.0.0.1 inside the container (supports services bound to loopback).
        // - For non-local destinations: only allow if explicitly enabled by policy.
        let dest_host = if is_localhost(host_to_connect) {
            "127.0.0.1".to_string()
        } else if self.server.config.port_forwarding.allow_nonlocal_destinations {
            host_to_connect.to_string()
        } else {
            warn!("Non-local destination {} denied by policy", host_to_connect);
            return Ok(false);
        };

        // Use socat inside the container to connect and bridge bytes. This avoids needing access to
        // the container's loopback from the gateway host (bridge networking).
        let cmd = vec![
            "socat".to_string(),
            "-".to_string(),
            format!("TCP:{}:{}", dest_host, port_to_connect),
        ];

        let exec_id = self
            .server
            .container_manager
            .create_exec(&container_id, cmd, false, None)
            .await?;

        // Treat direct-tcpip as a raw byte stream: no exit-status and no SSH stderr extended-data.
        self.start_exec_session(channel.id(), exec_id, false, ChannelStreamKind::TcpForward, session)
            .await?;

        Ok(true)
    }

    /// Handle tcpip-forward request (remote port forward).
    async fn tcpip_forward(
        &mut self,
        address: &str,
        port: &mut u32,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        if !self.server.config.port_forwarding.allow_remote {
            warn!("Remote port forwarding disabled");
            return Ok(false);
        }

        // Determine bind address
        let bind_addr = if address.is_empty() || address == "0.0.0.0" || address == "*" {
            if self.server.config.port_forwarding.allow_gateway_ports {
                "0.0.0.0"
            } else {
                "127.0.0.1"
            }
        } else if is_localhost(address) {
            "127.0.0.1"
        } else if self.server.config.port_forwarding.allow_gateway_ports {
            address
        } else {
            warn!("GatewayPorts disabled, binding to localhost");
            "127.0.0.1"
        };

        let listen_addr = format!("{}:{}", bind_addr, port);
        info!("Starting remote forward on {}", listen_addr);

        match TcpListener::bind(&listen_addr).await {
            Ok(listener) => {
                // If port was 0, get the actual port
                if *port == 0 {
                    if let Ok(addr) = listener.local_addr() {
                        *port = addr.port() as u32;
                    }
                }

                let handle = session.handle();
                let original_port = *port;
                let address_for_insert = address.to_string();
                let address_for_task = address.to_string();

                let task = tokio::spawn(async move {
                    loop {
                        match listener.accept().await {
                            Ok((stream, peer)) => {
                                let handle = handle.clone();
                                let address = address_for_task.clone();
                                tokio::spawn(async move {
                                    // Open forwarded-tcpip channel back to client
                                    match handle
                                        .channel_open_forwarded_tcpip(
                                            address,
                                            original_port,
                                            peer.ip().to_string(),
                                            peer.port() as u32,
                                        )
                                        .await
                                    {
                                        Ok(channel) => {
                                            // Relay data
                                            let (mut read_half, _write_half) = stream.into_split();
                                            let channel = channel;

                                            let read_task = async {
                                                let mut buf = vec![0u8; 32768];
                                                loop {
                                                    match read_half.read(&mut buf).await {
                                                        Ok(0) => break,
                                                        Ok(n) => {
                                                            if channel.data(&buf[..n]).await.is_err() {
                                                                break;
                                                            }
                                                        }
                                                        Err(_) => break,
                                                    }
                                                }
                                                let _ = channel.eof().await;
                                            };

                                            read_task.await;
                                        }
                                        Err(e) => {
                                            warn!("Failed to open forwarded-tcpip channel: {}", e);
                                        }
                                    }
                                });
                            }
                            Err(e) => {
                                warn!("Accept error: {}", e);
                                break;
                            }
                        }
                    }
                });

                self.remote_forwards
                    .insert((address_for_insert, *port), task);

                Ok(true)
            }
            Err(e) => {
                warn!("Failed to bind {}: {}", listen_addr, e);
                Ok(false)
            }
        }
    }

    /// Handle cancel-tcpip-forward request.
    async fn cancel_tcpip_forward(
        &mut self,
        address: &str,
        port: u32,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        if let Some(task) = self.remote_forwards.remove(&(address.to_string(), port)) {
            task.abort();
            info!("Cancelled remote forward on {}:{}", address, port);
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

impl ConnectionHandler {
    /// Cache all offered keys for a GitHub user.
    ///
    /// This ensures that all keys the client offered during auth are cached,
    /// not just the one that was verified against GitHub. This prevents
    /// repeated keyboard-interactive prompts when the client offers keys
    /// in a different order on reconnect.
    async fn cache_all_offered_keys(&self, github_user: &str, key_type: &str) {
        for fingerprint in &self.offered_key_fingerprints {
            // Skip if already cached
            if self.server.state.get_github_user(fingerprint).await.is_some() {
                continue;
            }

            let entry = KeyCacheEntry {
                github_username: github_user.to_string(),
                verified_at: Utc::now(),
                key_type: key_type.to_string(),
            };

            if let Err(e) = self.server.state.cache_key(fingerprint.clone(), entry).await {
                warn!("Failed to cache key {}: {}", fingerprint, e);
            } else {
                info!("Cached key {} for GitHub user '{}'", fingerprint, github_user);
            }
        }
    }

    /// Start an exec session and connect it to an SSH channel.
    async fn start_exec_session(
        &mut self,
        channel_id: ChannelId,
        exec_id: String,
        tty: bool,
        kind: ChannelStreamKind,
        session: &mut Session,
    ) -> Result<()> {
        let docker = self.server.container_manager.docker().clone();

        // Start the exec
        let results = self
            .server
            .container_manager
            .start_exec(&exec_id, tty)
            .await?;

        // Create channel for stdin
        let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(32);

        self.exec_sessions.insert(
            channel_id,
            ExecSession {
                exec_id: exec_id.clone(),
                tty,
                stdin_tx: Some(stdin_tx),
            },
        );

        // Get session handle for async operations
        let handle = session.handle();

        // Spawn task to handle the exec I/O
        tokio::spawn(async move {
            match results {
                StartExecResults::Attached { mut output, mut input } => {
                    // Task to forward stdin to container
                    let stdin_task = async move {
                        while let Some(data) = stdin_rx.recv().await {
                            if input.write_all(&data).await.is_err() {
                                break;
                            }
                        }
                    };

                    // Task to forward container output to SSH channel
                    let stdout_task = async move {
                        while let Some(output_result) = output.next().await {
                            match output_result {
                                Ok(output) => {
                                    match output {
                                        LogOutput::StdErr { message } => {
                                            match kind {
                                                ChannelStreamKind::Session => {
                                                    // Keep stderr separate so tools like Zed can use stdout as a clean transport.
                                                    if handle
                                                        .extended_data(
                                                            channel_id,
                                                            1, // SSH_EXTENDED_DATA_STDERR
                                                            CryptoVec::from_slice(message.as_ref()),
                                                        )
                                                        .await
                                                        .is_err()
                                                    {
                                                        break;
                                                    }
                                                }
                                                ChannelStreamKind::TcpForward => {
                                                    // For TCP forwarding channels, do not send stderr as it would corrupt the byte stream.
                                                    // Log it server-side instead.
                                                    warn!(
                                                        "tcp-forward stderr (ignored): {}",
                                                        String::from_utf8_lossy(message.as_ref())
                                                    );
                                                }
                                            }
                                        }
                                        LogOutput::StdOut { message }
                                        | LogOutput::StdIn { message }
                                        | LogOutput::Console { message } => {
                                            if handle
                                                .data(
                                                    channel_id,
                                                    CryptoVec::from_slice(message.as_ref()),
                                                )
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!("Exec output error: {}", e);
                                    break;
                                }
                            }
                        }

                        if kind == ChannelStreamKind::Session {
                            // Capture exit status for clients (editors) that rely on it.
                            // `inspect_exec` may briefly report Running=true even after the output stream ends,
                            // so we poll for a short time.
                            let mut exit_status: u32 = 255;
                            for _ in 0..80 {
                                match docker.inspect_exec(&exec_id).await {
                                    Ok(info) => {
                                        if info.running.unwrap_or(false) {
                                            tokio::time::sleep(Duration::from_millis(25)).await;
                                            continue;
                                        }
                                        let code = info.exit_code.unwrap_or(0);
                                        exit_status = if code < 0 { 255 } else { code as u32 };
                                        break;
                                    }
                                    Err(e) => {
                                        warn!("Failed to inspect exec {}: {}", exec_id, e);
                                        break;
                                    }
                                }
                            }

                            let _ = handle.exit_status_request(channel_id, exit_status).await;
                        }

                        // Send EOF and close
                        let _ = handle.eof(channel_id).await;
                        let _ = handle.close(channel_id).await;
                    };

                    // Keep forwarding stdout even if the client closes stdin early (common for `ssh -T ... cmd`).
                    let stdin_handle = tokio::spawn(stdin_task);
                    stdout_task.await;
                    stdin_handle.abort();
                }
                StartExecResults::Detached => {
                    warn!("Exec started in detached mode unexpectedly");
                }
            }
        });

        Ok(())
    }
}

/// Check if a hostname refers to localhost.
fn is_localhost(host: &str) -> bool {
    host == "localhost"
        || host == "127.0.0.1"
        || host == "::1"
        || host == "[::1]"
        || host == "0.0.0.0"
}

fn sanitize_tmux_session_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "agentman".to_string()
    } else {
        out
    }
}

#[derive(Debug, Clone, Copy)]
enum GatewayControlCommand {
    Help,
    Destroy {
        yes: bool,
        keep_workspace: bool,
        dry_run: bool,
        force: bool,
    },
}

fn parse_gateway_control_command(cmd: &str) -> Option<GatewayControlCommand> {
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

fn gateway_control_help_text() -> String {
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

/// Run the SSH server.
pub async fn run_server(
    config: Arc<GatewayConfig>,
    state: Arc<StateManager>,
    container_manager: Arc<ContainerManager>,
    github_fetcher: Arc<GitHubKeyFetcher>,
) -> Result<()> {
    // Load or generate host key
    let key = load_or_generate_host_key(&config.host_key_path).await?;

    let russh_config = Arc::new(russh::server::Config {
        auth_rejection_time: Duration::from_secs(1),
        auth_rejection_time_initial: Some(Duration::from_secs(0)),
        keys: vec![key],
        ..Default::default()
    });

    let server_state = Arc::new(ServerState {
        config: config.clone(),
        state,
        container_manager,
        github_fetcher,
    });

    let addr: SocketAddr = config
        .listen_addr
        .parse()
        .with_context(|| format!("Invalid listen address: {}", config.listen_addr))?;

    info!("SSH server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("SSH server listening on {}", listener.local_addr()?);

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let server_state_clone = server_state.clone();
        let russh_config_clone = russh_config.clone();

        tokio::spawn(async move {
            let handler = ConnectionHandler::new(server_state_clone, peer_addr);
            match russh::server::run_stream(russh_config_clone, stream, handler).await {
                Ok(session) => {
                    if let Err(e) = session.await {
                        warn!("SSH session error: {}", e);
                    }
                }
                Err(e) => {
                    warn!("SSH connection error: {}", e);
                }
            }
        });
    }
}

/// Load host key from file or generate a new one.
async fn load_or_generate_host_key(path: &std::path::Path) -> Result<russh::keys::PrivateKey> {
    use russh::keys::ssh_key::{Algorithm, LineEnding};
    use russh::keys::ssh_key::rand_core::OsRng;
    
    if path.exists() {
        info!("Loading host key from {}", path.display());
        let key = russh::keys::load_secret_key(path, None)
            .with_context(|| format!("Failed to load host key from {}", path.display()))?;
        Ok(key)
    } else {
        info!("Generating new Ed25519 host key");
        let key = russh::keys::PrivateKey::random(&mut OsRng, Algorithm::Ed25519)
            .context("Failed to generate host key")?;

        // Save the key
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Write key to file using OpenSSH format
        let key_bytes = key.to_openssh(LineEnding::LF)
            .context("Failed to encode host key")?;
        tokio::fs::write(path, key_bytes.as_bytes()).await?;

        // Set restrictive permissions (Unix only)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(path, perms)?;
        }

        info!("Saved host key to {}", path.display());
        Ok(key)
    }
}
