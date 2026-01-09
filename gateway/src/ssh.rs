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
use chrono::Utc;
use futures::StreamExt;
use russh::server::{Auth, Handler, Msg, Session};
use russh::{Channel, ChannelId, CryptoVec, MethodKind, MethodSet};
use russh::keys::PublicKey;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::GatewayConfig;
use crate::docker::ContainerManager;
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

    /// PTY dimensions (cols, rows) from pty_request.
    pty_size: Option<(u32, u32)>,

    /// Terminal type from pty_request.
    term: Option<String>,
}

struct ExecSession {
    exec_id: String,
    /// Channel for sending data to the container.
    stdin_tx: Option<mpsc::Sender<Vec<u8>>>,
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
            pty_size: None,
            term: None,
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
        _channel_id: ChannelId,
        term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!(
            "PTY request: term={}, cols={}, rows={}",
            term, col_width, row_height
        );
        // Store PTY dimensions for use when creating exec
        self.pty_size = Some((col_width, row_height));
        self.term = Some(term.to_string());
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

        // Use stored TERM from pty_request, or default
        let term = self.term.as_deref().unwrap_or("xterm-256color");

        // Create exec in container
        let exec_id = self
            .server
            .container_manager
            .create_exec(
                &container_id,
                vec!["bash".to_string(), "-l".to_string()],
                true,
                Some(vec![format!("TERM={}", term)]),
            )
            .await?;

        // Start exec and connect to channel
        self.start_exec_session(channel_id, exec_id.clone(), session).await?;

        // Resize to stored PTY dimensions
        if let Some((cols, rows)) = self.pty_size {
            if let Err(e) = self
                .server
                .container_manager
                .resize_exec(&exec_id, cols as u16, rows as u16)
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

        // Get or create container
        let container_id = self
            .server
            .container_manager
            .get_or_create_container(github_user, project)
            .await?;

        self.container_id = Some(container_id.clone());

        // Use stored TERM from pty_request, or default
        let term = self.term.as_deref().unwrap_or("xterm-256color");

        // Create exec in container
        let exec_id = self
            .server
            .container_manager
            .create_exec(
                &container_id,
                vec!["bash".to_string(), "-lc".to_string(), command],
                true,
                Some(vec![format!("TERM={}", term)]),
            )
            .await?;

        // Start exec and connect to channel
        self.start_exec_session(channel_id, exec_id.clone(), session).await?;

        // Resize to stored PTY dimensions
        if let Some((cols, rows)) = self.pty_size {
            if let Err(e) = self
                .server
                .container_manager
                .resize_exec(&exec_id, cols as u16, rows as u16)
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

        if let Some(exec_session) = self.exec_sessions.get(&channel_id) {
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
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        if !self.server.config.port_forwarding.allow_local {
            warn!("Local port forwarding disabled");
            return Ok(false);
        }

        info!(
            "Direct-tcpip request: {}:{} from {}:{}",
            host_to_connect, port_to_connect, originator_address, originator_port
        );

        // Determine target address
        let target_addr = if is_localhost(host_to_connect) {
            // Forward to container's IP
            if let Some(ref container_id) = self.container_id {
                match self
                    .server
                    .container_manager
                    .get_container_ip(container_id)
                    .await
                {
                    Ok(ip) => format!("{}:{}", ip, port_to_connect),
                    Err(e) => {
                        warn!("Failed to get container IP: {}", e);
                        return Ok(false);
                    }
                }
            } else {
                warn!("No container for port forward");
                return Ok(false);
            }
        } else if self.server.config.port_forwarding.allow_nonlocal_destinations {
            format!("{}:{}", host_to_connect, port_to_connect)
        } else {
            warn!(
                "Non-local destination {} denied by policy",
                host_to_connect
            );
            return Ok(false);
        };

        // Spawn task to handle the forward
        let _channel_id = channel.id();
        tokio::spawn(async move {
            match TcpStream::connect(&target_addr).await {
                Ok(mut stream) => {
                    let (mut read_half, mut write_half) = stream.split();
                    let channel = channel;

                    // Forward data in both directions
                    let (_tx, mut rx) = mpsc::channel::<Vec<u8>>(32);

                    // Channel -> TCP
                    let write_task = async {
                        while let Some(data) = rx.recv().await {
                            if write_half.write_all(&data).await.is_err() {
                                break;
                            }
                        }
                    };

                    // TCP -> Channel
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

                    tokio::select! {
                        _ = write_task => {}
                        _ = read_task => {}
                    }
                }
                Err(e) => {
                    warn!("Failed to connect to {}: {}", target_addr, e);
                }
            }
        });

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
        session: &mut Session,
    ) -> Result<()> {
        let _docker = self.server.container_manager.docker().clone();

        // Start the exec
        let results = self
            .server
            .container_manager
            .start_exec(&exec_id)
            .await?;

        // Create channel for stdin
        let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(32);

        self.exec_sessions.insert(
            channel_id,
            ExecSession {
                exec_id: exec_id.clone(),
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
                    let stdin_task = async {
                        while let Some(data) = stdin_rx.recv().await {
                            if input.write_all(&data).await.is_err() {
                                break;
                            }
                        }
                    };

                    // Task to forward container output to SSH channel
                    let stdout_task = async {
                        while let Some(output_result) = output.next().await {
                            match output_result {
                                Ok(output) => {
                                    let data = output.into_bytes();
                                    if handle
                                        .data(channel_id, CryptoVec::from_slice(&data))
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    warn!("Exec output error: {}", e);
                                    break;
                                }
                            }
                        }
                        // Send EOF and close
                        let _ = handle.eof(channel_id).await;
                        let _ = handle.close(channel_id).await;
                    };

                    tokio::select! {
                        _ = stdin_task => {}
                        _ = stdout_task => {}
                    }
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
