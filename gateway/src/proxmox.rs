//! Proxmox VE LXC container provisioning and management.
//!
//! Handles:
//! - Creating unprivileged LXC containers from a template via the Proxmox VE API
//! - Bind-mounting persistent workspaces (via `pct`, since only root@pam may add bind mounts)
//! - Container lifecycle (start, stop, freeze, destroy)
//! - Attaching shells/commands with `lxc-attach`
//!
//! The gateway is expected to run as root on the Proxmox node that hosts the containers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use chrono::Utc;
use futures::stream::{self, BoxStream};
use futures::{FutureExt, StreamExt};
use reqwest::Method;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::config::{GatewayConfig, ProxmoxConfig, RuntimeKind};
use crate::docker::{ensure_workspace_writable, parse_memory_limit};
use crate::pty::{make_controlling_terminal, open_pty, PtyReader, PtyWriter};
use crate::runtime::{
    DestroyOptions, DestroyResult, ExecOutput, ExecProcess, ExecSpec, Resizer, SandboxStats,
    SandboxStatus,
};
use crate::state::{StateManager, WorkspaceInfo};

/// Tag applied to every container agentman creates.
const AGENTMAN_TAG: &str = "agentman";

/// Where the workspace is mounted inside the container.
const WORKSPACE_MOUNT: &str = "/workspace";

/// Minimal Proxmox VE API client authenticated with an API token.
struct ProxmoxApi {
    client: reqwest::Client,
    base_url: String,
    auth_header: String,
}

impl ProxmoxApi {
    fn new(cfg: &ProxmoxConfig) -> Result<Self> {
        let secret = load_token_secret(cfg)?;

        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            // The API is normally on this host; never route it through an HTTP proxy.
            .no_proxy();

        if cfg.insecure_skip_tls_verify {
            warn!("Proxmox API TLS verification is disabled");
            builder = builder.danger_accept_invalid_certs(true);
        } else if let Some(ref ca) = cfg.ca_cert {
            if ca.exists() {
                let pem = std::fs::read(ca)
                    .with_context(|| format!("Failed to read Proxmox CA cert {}", ca.display()))?;
                let cert = reqwest::Certificate::from_pem(&pem)
                    .with_context(|| format!("Invalid Proxmox CA cert {}", ca.display()))?;
                builder = builder.add_root_certificate(cert);
            } else {
                warn!(
                    "Proxmox CA cert {} not found; using system trust roots",
                    ca.display()
                );
            }
        }

        Ok(Self {
            client: builder.build().context("Failed to build Proxmox API client")?,
            base_url: format!("{}/api2/json", cfg.api_url.trim_end_matches('/')),
            auth_header: format!("PVEAPIToken={}={}", cfg.token_id, secret),
        })
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<Value> {
        let url = format!("{}{}", self.base_url, path);
        let mut req = self
            .client
            .request(method.clone(), &url)
            .header("Authorization", &self.auth_header);
        req = if method == Method::GET || method == Method::DELETE {
            req.query(params)
        } else {
            req.form(params)
        };

        let resp = req
            .send()
            .await
            .with_context(|| format!("Proxmox API {method} {path} failed"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            bail!(
                "Proxmox API {method} {path} returned {status}: {}",
                api_error_message(&body)
            );
        }

        let v: Value = serde_json::from_str(&body)
            .with_context(|| format!("Proxmox API {method} {path}: invalid JSON response"))?;
        Ok(v.get("data").cloned().unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> Result<Value> {
        self.request(Method::GET, path, &[]).await
    }

    async fn post(&self, path: &str, params: &[(&str, String)]) -> Result<Value> {
        self.request(Method::POST, path, params).await
    }

    async fn delete(&self, path: &str, params: &[(&str, String)]) -> Result<Value> {
        self.request(Method::DELETE, path, params).await
    }

    /// Wait for an asynchronous task (UPID) to finish and check that it succeeded.
    async fn wait_task(&self, node: &str, upid: &Value, max_wait: Duration) -> Result<()> {
        let upid = upid
            .as_str()
            .ok_or_else(|| anyhow!("Proxmox API did not return a task id"))?;
        let path = format!(
            "/nodes/{}/tasks/{}/status",
            encode_path_segment(node),
            encode_path_segment(upid)
        );

        let deadline = tokio::time::Instant::now() + max_wait;
        loop {
            let st = self.get(&path).await?;
            if st.get("status").and_then(Value::as_str) == Some("stopped") {
                let exit = st
                    .get("exitstatus")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                if exit == "OK" || exit.starts_with("WARNINGS") {
                    return Ok(());
                }
                bail!("Proxmox task {upid} failed: {exit}");
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("Timed out waiting for Proxmox task {upid}");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

fn load_token_secret(cfg: &ProxmoxConfig) -> Result<String> {
    if let Ok(secret) = std::env::var("AGENTMAN_PROXMOX_TOKEN_SECRET") {
        let secret = secret.trim().to_string();
        if !secret.is_empty() {
            return Ok(secret);
        }
    }
    let path = cfg.token_secret_file.as_ref().ok_or_else(|| {
        anyhow!("No Proxmox API token secret: set proxmox.token_secret_file or AGENTMAN_PROXMOX_TOKEN_SECRET")
    })?;
    let secret = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read Proxmox token secret file {}", path.display()))?;
    let secret = secret.trim().to_string();
    if secret.is_empty() {
        bail!("Proxmox token secret file {} is empty", path.display());
    }
    Ok(secret)
}

/// Extract a readable message from a Proxmox API error body.
fn api_error_message(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return body.trim().to_string();
    };
    let mut parts = Vec::new();
    if let Some(msg) = v.get("message").and_then(Value::as_str) {
        parts.push(msg.trim().to_string());
    }
    if let Some(errors) = v.get("errors").and_then(Value::as_object) {
        for (k, e) in errors {
            parts.push(format!("{k}: {}", e.as_str().unwrap_or_default().trim()));
        }
    }
    if parts.is_empty() {
        body.trim().to_string()
    } else {
        parts.join("; ")
    }
}

/// Percent-encode a URL path segment (UPIDs contain ':' and '@', token ids contain '!').
fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Turn an arbitrary name into a valid DNS label for the container hostname.
fn sanitize_hostname(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    out.truncate(63);
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "agentman".to_string()
    } else {
        out
    }
}

/// Short hostname of this machine, which is the Proxmox node name.
fn local_node_name() -> Result<String> {
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .context("Failed to determine local hostname; set proxmox.node")?;
    let short = hostname.trim().split('.').next().unwrap_or("").to_string();
    if short.is_empty() {
        bail!("Local hostname is empty; set proxmox.node");
    }
    Ok(short)
}

/// Parse a Proxmox numeric field that may be encoded as a JSON number or string.
fn value_u64(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_f64().map(|f| f as u64))
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// Merge environment lists (`KEY=VALUE`); later entries override earlier ones.
fn merge_env(lists: &[&[String]]) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    let mut values: HashMap<String, String> = HashMap::new();
    for list in lists {
        for entry in list.iter() {
            let key = entry.split('=').next().unwrap_or(entry).to_string();
            if !values.contains_key(&key) {
                order.push(key.clone());
            }
            values.insert(key, entry.clone());
        }
    }
    order.into_iter().filter_map(|k| values.remove(&k)).collect()
}

/// Build the `lxc-attach` argument list for running `spec` in container `vmid`.
fn lxc_attach_args(cfg: &ProxmoxConfig, vmid: &str, env: &[String], cmd: &[String]) -> Vec<String> {
    let mut args = vec!["-n".to_string(), vmid.to_string(), "--clear-env".to_string()];
    if !cfg.exec_init_groups {
        // lxc-attach switches user itself but drops supplementary groups.
        args.extend([
            "--uid".to_string(),
            cfg.exec_uid.to_string(),
            "--gid".to_string(),
            cfg.exec_gid.to_string(),
        ]);
    }
    for kv in env {
        args.push("--set-var".to_string());
        args.push(kv.clone());
    }
    args.push("--".to_string());
    if cfg.exec_init_groups {
        // Attach as container root, then drop to the exec user with its groups (e.g. docker).
        args.extend([
            "setpriv".to_string(),
            format!("--reuid={}", cfg.exec_uid),
            format!("--regid={}", cfg.exec_gid),
            "--init-groups".to_string(),
        ]);
    }
    // lxc-attach has no working-directory option, so change into the workspace first.
    args.extend([
        "/bin/sh".to_string(),
        "-c".to_string(),
        format!("cd {WORKSPACE_MOUNT} 2>/dev/null || cd /; exec \"$@\""),
        "agentman".to_string(),
    ]);
    args.extend(cmd.iter().cloned());
    args
}

/// Read a stream of byte chunks from an async reader.
fn read_chunks<R>(reader: R, wrap: fn(Bytes) -> ExecOutput) -> BoxStream<'static, std::io::Result<ExecOutput>>
where
    R: AsyncRead + Send + Unpin + 'static,
{
    stream::unfold(Some(reader), move |reader| async move {
        let mut reader = reader?;
        let mut buf = vec![0u8; 32 * 1024];
        match reader.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(wrap(Bytes::from(buf))), Some(reader)))
            }
            Err(e) => Some((Err(e), None)),
        }
    })
    .boxed()
}

fn exit_code(status: std::io::Result<std::process::ExitStatus>) -> u32 {
    use std::os::unix::process::ExitStatusExt;
    match status {
        Ok(st) => match (st.code(), st.signal()) {
            (Some(code), _) => code as u32,
            (None, Some(sig)) => 128 + sig as u32,
            _ => 255,
        },
        Err(e) => {
            warn!("Failed to wait for lxc-attach: {}", e);
            255
        }
    }
}

/// An LXC container as reported by `GET /nodes/{node}/lxc`.
#[derive(Debug, Clone)]
struct CtInfo {
    vmid: String,
    name: String,
    status: String,
    tags: Vec<String>,
    cpu: Option<f64>,
    cpus: Option<f64>,
    mem: Option<u64>,
    maxmem: Option<u64>,
}

impl CtInfo {
    fn from_value(v: &Value) -> Option<Self> {
        Some(Self {
            vmid: value_u64(v.get("vmid")?)?.to_string(),
            name: v.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
            status: v
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            tags: v
                .get("tags")
                .and_then(Value::as_str)
                .unwrap_or("")
                .split([';', ',', ' '])
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .collect(),
            cpu: v.get("cpu").and_then(Value::as_f64),
            cpus: v.get("cpus").and_then(|c| c.as_f64().or_else(|| value_u64(c).map(|n| n as f64))),
            mem: v.get("mem").and_then(value_u64),
            maxmem: v.get("maxmem").and_then(value_u64),
        })
    }

    fn is_managed(&self) -> bool {
        self.tags.iter().any(|t| t == AGENTMAN_TAG)
    }
}

/// Proxmox LXC container manager.
pub struct ProxmoxManager {
    api: ProxmoxApi,
    node: String,
    config: Arc<GatewayConfig>,
    state: Arc<StateManager>,
    /// Per-container environment (GITHUB_USERNAME etc.), the LXC analogue of Docker's container env.
    container_env: RwLock<HashMap<String, Vec<String>>>,
}

impl ProxmoxManager {
    /// Create a new manager and verify the Proxmox API is reachable.
    pub async fn new(config: Arc<GatewayConfig>, state: Arc<StateManager>) -> Result<Self> {
        let pcfg = &config.proxmox;
        let api = ProxmoxApi::new(pcfg)?;
        let node = if pcfg.node.is_empty() {
            local_node_name()?
        } else {
            pcfg.node.clone()
        };

        let version = api
            .get("/version")
            .await
            .context("Failed to reach the Proxmox VE API")?;
        let version = version
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        info!("Connected to Proxmox VE {} (node {})", version, node);

        let lxc_attach = pcfg.lxc_bin_dir.join("lxc-attach");
        if !lxc_attach.exists() {
            warn!(
                "{} not found: the gateway must run on the Proxmox node that hosts the containers",
                lxc_attach.display()
            );
        }

        Ok(Self {
            api,
            node,
            config,
            state,
            container_env: RwLock::new(HashMap::new()),
        })
    }

    fn pcfg(&self) -> &ProxmoxConfig {
        &self.config.proxmox
    }

    fn lxc_tool(&self, name: &str) -> PathBuf {
        self.pcfg().lxc_bin_dir.join(name)
    }

    fn ct_path(&self, vmid: &str, suffix: &str) -> String {
        format!(
            "/nodes/{}/lxc/{}{}",
            encode_path_segment(&self.node),
            encode_path_segment(vmid),
            suffix
        )
    }

    async fn list_cts(&self) -> Result<Vec<CtInfo>> {
        let data = self
            .api
            .get(&format!("/nodes/{}/lxc", encode_path_segment(&self.node)))
            .await?;
        Ok(data
            .as_array()
            .map(|a| a.iter().filter_map(CtInfo::from_value).collect())
            .unwrap_or_default())
    }

    async fn find_ct(&self, vmid: &str) -> Result<Option<CtInfo>> {
        Ok(self.list_cts().await?.into_iter().find(|c| c.vmid == vmid))
    }

    /// Whether a running container is frozen (`lxc-freeze`).
    async fn is_frozen(&self, vmid: &str) -> bool {
        match Command::new(self.lxc_tool("lxc-info"))
            .args(["-n", vmid, "-s", "-H"])
            .output()
            .await
        {
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).trim() == "FROZEN"
            }
            Ok(out) => {
                debug!(
                    "lxc-info -n {} failed: {}",
                    vmid,
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                false
            }
            Err(e) => {
                debug!("Failed to run lxc-info: {}", e);
                false
            }
        }
    }

    async fn run_tool(&self, program: &Path, args: &[&str]) -> Result<()> {
        let out = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .await
            .with_context(|| format!("Failed to run {}", program.display()))?;
        if !out.status.success() {
            bail!(
                "{} {} failed: {}",
                program.display(),
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// Get or create a container for the given user and project. Returns the VMID.
    pub async fn get_or_create_container(&self, github_user: &str, project: &str) -> Result<String> {
        // Ensure the host workspace directory is writable by the container user (needed for Zed/VS Code bootstraps).
        let workspace_path = self.config.workspace_path(github_user, project);
        ensure_workspace_writable(&workspace_path, self.config.workspace_owner()).await?;

        if let Some(workspace) = self.state.get_workspace(github_user, project).await {
            if workspace.runtime != RuntimeKind::Proxmox {
                warn!(
                    "Workspace {}/{} belongs to the {} runtime; creating an LXC container",
                    github_user, project, workspace.runtime
                );
            } else if let Some(ref vmid) = workspace.container_id {
                if let Some(ct) = self.find_ct(vmid).await? {
                    self.ensure_running(&ct).await?;
                    self.remember_env(vmid, github_user, project, &workspace.container_name)
                        .await;
                    return Ok(vmid.clone());
                }
            }
            warn!(
                "Container {} no longer exists, recreating",
                workspace.container_name
            );
        }

        self.create_container(github_user, project).await
    }

    async fn remember_env(&self, vmid: &str, github_user: &str, project: &str, name: &str) {
        let env = vec![
            format!("GITHUB_USERNAME={}", github_user),
            format!("AGENTMAN_PROJECT={}", project),
            format!("AGENTMAN_CONTAINER_ID={}", name),
            "TERM=xterm-256color".to_string(),
        ];
        self.container_env.write().await.insert(vmid.to_string(), env);
    }

    async fn ensure_running(&self, ct: &CtInfo) -> Result<()> {
        if ct.status == "running" {
            // A frozen container still reports "running" but attach would hang.
            if self.is_frozen(&ct.vmid).await {
                info!("Unfreezing paused container {}", ct.vmid);
                self.run_tool(&self.lxc_tool("lxc-unfreeze"), &["-n", &ct.vmid])
                    .await?;
            }
            return Ok(());
        }

        info!("Starting stopped container {}", ct.vmid);
        let upid = self
            .api
            .post(&self.ct_path(&ct.vmid, "/status/start"), &[])
            .await
            .context("Failed to start container")?;
        self.api
            .wait_task(&self.node, &upid, Duration::from_secs(120))
            .await
    }

    async fn unique_hostname(&self, base: &str) -> Result<String> {
        let names: Vec<String> = self.list_cts().await?.into_iter().map(|c| c.name).collect();
        let base = sanitize_hostname(base);
        let mut name = base.clone();
        let mut suffix = 0;
        while names.contains(&name) {
            suffix += 1;
            if suffix > 100 {
                bail!("Could not find unique container name after 100 attempts");
            }
            let tail = format!("-{suffix}");
            let mut head = base.clone();
            head.truncate(63 - tail.len());
            name = format!("{}{}", head.trim_end_matches('-'), tail);
        }
        Ok(name)
    }

    fn create_params(&self, vmid: &str, hostname: &str, github_user: &str, project: &str) -> Result<Vec<(&'static str, String)>> {
        let pcfg = self.pcfg();
        let security = &self.config.container_security;

        let memory_mb = match security.memory_limit {
            Some(ref m) => (parse_memory_limit(m)? / (1024 * 1024)).max(16) as u64,
            None => pcfg.default_memory_mb,
        };

        let mut net0 = format!("name=eth0,bridge={},ip={}", pcfg.bridge, pcfg.ip);
        if let Some(ref gw) = pcfg.gateway {
            net0.push_str(&format!(",gw={gw}"));
        }

        let mut params: Vec<(&'static str, String)> = vec![
            ("vmid", vmid.to_string()),
            ("ostemplate", pcfg.ostemplate.clone()),
            ("hostname", hostname.to_string()),
            ("rootfs", format!("{}:{}", pcfg.storage, pcfg.rootfs_size_gb)),
            ("memory", memory_mb.to_string()),
            ("swap", pcfg.swap_mb.to_string()),
            ("net0", net0),
            ("unprivileged", if pcfg.unprivileged { "1" } else { "0" }.to_string()),
            ("tags", AGENTMAN_TAG.to_string()),
            (
                "description",
                format!("Managed by agentman for {github_user}/{project}"),
            ),
            ("onboot", "0".to_string()),
            ("start", "0".to_string()),
        ];
        if let Some(features) = pcfg.features() {
            params.push(("features", features));
        }
        if let Some(cpu) = security.cpu_limit {
            params.push(("cores", (cpu.ceil().max(1.0) as u64).to_string()));
            params.push(("cpulimit", format!("{cpu}")));
        }
        if let Some(ref pool) = pcfg.pool {
            params.push(("pool", pool.clone()));
        }
        Ok(params)
    }

    /// Create a new container for the given user and project.
    async fn create_container(&self, github_user: &str, project: &str) -> Result<String> {
        let now = Utc::now();
        let date_str = now.format("%Y%m%d").to_string();
        let hostname = self
            .unique_hostname(&format!("{}-{}-{}", project, github_user, date_str))
            .await?;

        let workspace_path = self.config.workspace_path(github_user, project);
        ensure_workspace_writable(&workspace_path, self.config.workspace_owner()).await?;

        // /cluster/nextid can race with other creators; retry with a fresh id if creation fails.
        let mut last_err = None;
        let mut vmid = String::new();
        for _ in 0..3 {
            let next = self.api.get("/cluster/nextid").await?;
            vmid = value_u64(&next)
                .ok_or_else(|| anyhow!("Unexpected /cluster/nextid response: {next}"))?
                .to_string();

            info!(
                "Creating LXC container {} ({}) for {}/{}",
                vmid, hostname, github_user, project
            );
            let params = self.create_params(&vmid, &hostname, github_user, project)?;
            let res = async {
                let upid = self
                    .api
                    .post(&format!("/nodes/{}/lxc", encode_path_segment(&self.node)), &params)
                    .await?;
                self.api
                    .wait_task(&self.node, &upid, Duration::from_secs(600))
                    .await
            }
            .await;
            match res {
                Ok(()) => {
                    last_err = None;
                    break;
                }
                Err(e) if format!("{e:#}").contains("already exists") => {
                    warn!("VMID {} was taken, retrying: {:#}", vmid, e);
                    last_err = Some(e);
                }
                Err(e) => return Err(e.context(format!("Failed to create container {hostname}"))),
            }
        }
        if let Some(e) = last_err {
            return Err(e.context(format!("Failed to create container {hostname}")));
        }

        // Bind mount the persistent workspace. Proxmox only lets root@pam add bind mounts,
        // so this goes through `pct` on the node rather than the API token.
        let mp0 = format!("{},mp={}", workspace_path.display(), WORKSPACE_MOUNT);
        let vmid_arg = vmid.as_str();
        if let Err(e) = self
            .run_tool(&self.pcfg().pct_path, &["set", vmid_arg, "--mp0", &mp0])
            .await
        {
            warn!("Removing container {} after failed setup", vmid);
            if let Err(e2) = self.delete_ct(&vmid).await {
                warn!("Cleanup of container {} failed: {:#}", vmid, e2);
            }
            return Err(e.context("Failed to bind-mount the workspace"));
        }

        let upid = self
            .api
            .post(&self.ct_path(&vmid, "/status/start"), &[])
            .await
            .with_context(|| format!("Failed to start container {hostname}"))?;
        self.api
            .wait_task(&self.node, &upid, Duration::from_secs(120))
            .await
            .with_context(|| format!("Failed to start container {hostname}"))?;
        info!("Started LXC container {} ({})", vmid, hostname);

        self.state
            .set_workspace(WorkspaceInfo {
                github_user: github_user.to_string(),
                project: project.to_string(),
                runtime: RuntimeKind::Proxmox,
                container_name: hostname.clone(),
                container_id: Some(vmid.clone()),
                created_at: now,
                host_workspace_path: workspace_path,
            })
            .await?;

        self.remember_env(&vmid, github_user, project, &hostname).await;
        Ok(vmid)
    }

    /// Start a process in the container with its I/O attached.
    pub async fn exec(&self, vmid: &str, spec: ExecSpec) -> Result<ExecProcess> {
        let pcfg = self.pcfg();
        let container_env = self
            .container_env
            .read()
            .await
            .get(vmid)
            .cloned()
            .unwrap_or_default();
        let env = merge_env(&[&pcfg.exec_env, &container_env, &spec.env]);
        let args = lxc_attach_args(pcfg, vmid, &env, &spec.cmd);

        let mut cmd = Command::new(self.lxc_tool("lxc-attach"));
        cmd.args(&args).kill_on_drop(true);

        if spec.tty {
            // Clients without a local terminal request a 0x0 PTY; fall back to a usable size.
            let (cols, rows) = spec
                .size
                .filter(|&(c, r)| c > 0 && r > 0)
                .unwrap_or((80, 24));
            let (master, slave) = open_pty(cols, rows)?;
            cmd.stdin(Stdio::from(slave.try_clone()?))
                .stdout(Stdio::from(slave.try_clone()?))
                .stderr(Stdio::from(slave));
            // SAFETY: the pre_exec hook only calls async-signal-safe functions.
            unsafe {
                cmd.pre_exec(make_controlling_terminal);
            }
            let mut child = cmd.spawn().context("Failed to run lxc-attach")?;
            // Close our copies of the slave so the master sees EOF when the process exits.
            drop(cmd);

            let master = Arc::new(master);
            Ok(ExecProcess {
                output: read_chunks(PtyReader(master.clone()), ExecOutput::Stdout),
                input: Box::pin(PtyWriter(master.clone())),
                exit_status: async move { exit_code(child.wait().await) }.boxed(),
                resizer: Resizer::Pty(master),
            })
        } else {
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = cmd.spawn().context("Failed to run lxc-attach")?;
            let stdin = child.stdin.take().ok_or_else(|| anyhow!("missing stdin"))?;
            let stdout = child.stdout.take().ok_or_else(|| anyhow!("missing stdout"))?;
            let stderr = child.stderr.take().ok_or_else(|| anyhow!("missing stderr"))?;

            let output = stream::select(
                read_chunks(stdout, ExecOutput::Stdout),
                read_chunks(stderr, ExecOutput::Stderr),
            )
            .boxed();

            Ok(ExecProcess {
                output,
                input: Box::pin(stdin),
                exit_status: async move { exit_code(child.wait().await) }.boxed(),
                resizer: Resizer::None,
            })
        }
    }

    /// List all workspaces for a given GitHub user.
    pub async fn list_workspaces(&self, github_user: &str) -> Vec<WorkspaceInfo> {
        self.state.list_workspaces(github_user).await
    }

    /// Get workspace info by (github_user, project).
    pub async fn get_workspace(&self, github_user: &str, project: &str) -> Option<WorkspaceInfo> {
        self.state.get_workspace(github_user, project).await
    }

    /// Current status of a workspace's container.
    pub async fn status(&self, ws: &WorkspaceInfo) -> SandboxStatus {
        let Some(ref vmid) = ws.container_id else {
            return SandboxStatus::missing();
        };
        match self.find_ct(vmid).await {
            Ok(Some(ct)) => {
                let running = ct.status == "running";
                let paused = running && self.is_frozen(vmid).await;
                SandboxStatus {
                    status: if paused {
                        "paused".to_string()
                    } else {
                        ct.status
                    },
                    id_short: Some(ct.vmid),
                    running,
                    paused,
                }
            }
            Ok(None) => SandboxStatus::missing(),
            Err(e) => {
                warn!("Failed to query container {}: {:#}", vmid, e);
                SandboxStatus::error()
            }
        }
    }

    /// Shut down a container (10s grace period, then force stop).
    pub async fn stop(&self, ws: &WorkspaceInfo) -> Result<()> {
        let vmid = ws
            .container_id
            .as_deref()
            .ok_or_else(|| anyhow!("workspace has no container id"))?;
        self.shutdown_ct(vmid, false).await
    }

    async fn shutdown_ct(&self, vmid: &str, force: bool) -> Result<()> {
        // A frozen container cannot shut down cleanly.
        if self.is_frozen(vmid).await {
            self.run_tool(&self.lxc_tool("lxc-unfreeze"), &["-n", vmid])
                .await?;
        }
        let upid = if force {
            self.api
                .post(&self.ct_path(vmid, "/status/stop"), &[])
                .await?
        } else {
            self.api
                .post(
                    &self.ct_path(vmid, "/status/shutdown"),
                    &[("timeout", "10".to_string()), ("forceStop", "1".to_string())],
                )
                .await?
        };
        self.api
            .wait_task(&self.node, &upid, Duration::from_secs(60))
            .await
    }

    /// Freeze a container (the LXC equivalent of `docker pause`).
    pub async fn pause(&self, ws: &WorkspaceInfo) -> Result<()> {
        let vmid = ws
            .container_id
            .as_deref()
            .ok_or_else(|| anyhow!("workspace has no container id"))?;
        self.run_tool(&self.lxc_tool("lxc-freeze"), &["-n", vmid])
            .await
    }

    /// Resource usage of a running container.
    pub async fn stats(&self, ws: &WorkspaceInfo) -> Option<SandboxStats> {
        let vmid = ws.container_id.as_deref()?;
        let ct = self.find_ct(vmid).await.ok()??;
        // Proxmox reports CPU as a fraction of the container's CPUs; report percent of one core
        // like `docker stats`.
        let cpu_percent = ct.cpu.map(|c| c * ct.cpus.unwrap_or(1.0) * 100.0);
        let mem = match (ct.mem, ct.maxmem) {
            (Some(u), Some(l)) if l > 0 => Some((u, l)),
            _ => None,
        };
        Some(SandboxStats { cpu_percent, mem })
    }

    async fn delete_ct(&self, vmid: &str) -> Result<()> {
        let upid = self
            .api
            .delete(
                &self.ct_path(vmid, ""),
                &[
                    ("purge", "1".to_string()),
                    ("destroy-unreferenced-disks", "1".to_string()),
                ],
            )
            .await?;
        self.api
            .wait_task(&self.node, &upid, Duration::from_secs(300))
            .await
    }

    /// Destroy a workspace:
    /// - Stop/remove any managed container(s) for (github_user, project)
    /// - Optionally delete the persistent workspace directory on the host
    /// - Remove the workspace entry from the gateway state file
    ///
    /// Proxmox never deletes bind-mount sources, so the workspace survives container removal.
    pub async fn destroy_workspace(
        &self,
        github_user: &str,
        project: &str,
        opts: DestroyOptions,
    ) -> Result<DestroyResult> {
        let mut warnings = Vec::new();
        let workspace_path = self.config.workspace_path(github_user, project);

        let cts = self.list_cts().await.context("Failed to list containers")?;

        // Targets: the state-mapped VMID, plus any agentman-tagged container with the same hostname.
        let mut targets: Vec<CtInfo> = Vec::new();
        if let Some(ws) = self.state.get_workspace(github_user, project).await {
            if ws.runtime != RuntimeKind::Proxmox {
                warnings.push(format!(
                    "workspace belongs to the {} runtime; its container was not removed",
                    ws.runtime
                ));
            } else {
                for ct in &cts {
                    let by_id = ws.container_id.as_deref() == Some(ct.vmid.as_str());
                    let by_name = ct.is_managed() && ct.name == ws.container_name;
                    if by_id || by_name {
                        targets.push(ct.clone());
                    }
                }
            }
        }
        targets.sort_by(|a, b| a.vmid.cmp(&b.vmid));
        targets.dedup_by(|a, b| a.vmid == b.vmid);

        let mut removed_containers = Vec::new();
        for ct in targets {
            let label = format!("{} ({})", ct.vmid, ct.name);
            if opts.dry_run {
                removed_containers.push(format!("{label} (dry-run)"));
                continue;
            }

            if ct.status == "running" {
                if let Err(e) = self.shutdown_ct(&ct.vmid, opts.force).await {
                    warnings.push(format!("stop container {label}: {e:#}"));
                }
            }

            match self.delete_ct(&ct.vmid).await {
                Ok(()) => removed_containers.push(label),
                Err(e) => warnings.push(format!("remove container {label}: {e:#}")),
            }
        }

        // Delete persistent workspace directory.
        let mut workspace_deleted = false;
        if !opts.keep_workspace && workspace_path.exists() {
            if !opts.dry_run {
                tokio::fs::remove_dir_all(&workspace_path)
                    .await
                    .with_context(|| {
                        format!(
                            "Failed to delete workspace directory: {}",
                            workspace_path.display()
                        )
                    })?;
            }
            workspace_deleted = true;
        }

        // Remove the workspace entry from state.
        let state_entry_deleted = if opts.dry_run {
            false
        } else {
            self.container_env.write().await.retain(|_, env| {
                !env.contains(&format!("GITHUB_USERNAME={github_user}"))
                    || !env.contains(&format!("AGENTMAN_PROJECT={project}"))
            });
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostname_is_a_dns_label() {
        assert_eq!(sanitize_hostname("my_proj-octocat-20261013"), "my-proj-octocat-20261013");
        assert_eq!(sanitize_hostname("Ünï_x"), "n--x");
        assert_eq!(sanitize_hostname("___"), "agentman");
        let long = sanitize_hostname(&"a".repeat(100));
        assert_eq!(long.len(), 63);
    }

    #[test]
    fn upid_is_percent_encoded() {
        assert_eq!(
            encode_path_segment("UPID:pve:00001:0A:6720:vzcreate:101:root@pam!agentman:"),
            "UPID%3Apve%3A00001%3A0A%3A6720%3Avzcreate%3A101%3Aroot%40pam%21agentman%3A"
        );
    }

    #[test]
    fn env_merge_later_wins_and_keeps_order() {
        let base = vec!["HOME=/home/agent".to_string(), "PATH=/bin".to_string()];
        let extra = vec!["HOME=/workspace".to_string(), "TERM=xterm".to_string()];
        assert_eq!(
            merge_env(&[&base, &extra]),
            vec!["HOME=/workspace", "PATH=/bin", "TERM=xterm"]
        );
    }

    #[test]
    fn attach_args_run_as_exec_user_in_workspace() {
        let cfg = ProxmoxConfig::default();
        let cmd = ["/bin/bash".to_string(), "-l".to_string()];
        let args = lxc_attach_args(&cfg, "105", &["A=1".to_string()], &cmd);
        assert_eq!(
            args,
            vec![
                "-n", "105", "--clear-env", "--set-var", "A=1", "--",
                "setpriv", "--reuid=1000", "--regid=1000", "--init-groups",
                "/bin/sh", "-c",
                "cd /workspace 2>/dev/null || cd /; exec \"$@\"",
                "agentman", "/bin/bash", "-l",
            ]
        );

        let no_groups = ProxmoxConfig {
            exec_init_groups: false,
            ..ProxmoxConfig::default()
        };
        let args = lxc_attach_args(&no_groups, "105", &[], &cmd);
        assert_eq!(
            &args[..8],
            ["-n", "105", "--clear-env", "--uid", "1000", "--gid", "1000", "--"]
        );
        assert_eq!(args[8], "/bin/sh");
    }

    #[test]
    fn ct_info_parses_api_listing() {
        let v: Value = serde_json::json!({
            "vmid": 105, "name": "proj-octocat-20261013", "status": "running",
            "tags": "agentman;dev", "cpu": 0.25, "cpus": 4, "mem": 1048576, "maxmem": 4194304
        });
        let ct = CtInfo::from_value(&v).unwrap();
        assert_eq!(ct.vmid, "105");
        assert!(ct.is_managed());
        assert_eq!(ct.cpus, Some(4.0));
        assert_eq!(ct.maxmem, Some(4194304));
    }

    #[test]
    fn features_allow_docker_inside() {
        assert_eq!(
            ProxmoxConfig::default().features().as_deref(),
            Some("nesting=1,keyctl=1")
        );
        let privileged = ProxmoxConfig {
            unprivileged: false,
            ..ProxmoxConfig::default()
        };
        assert_eq!(privileged.features().as_deref(), Some("nesting=1"));
    }

    #[test]
    fn unprivileged_workspace_owner_is_shifted() {
        let cfg = ProxmoxConfig::default();
        assert_eq!(cfg.host_owner(), (101000, 101000));
        let privileged = ProxmoxConfig {
            unprivileged: false,
            ..ProxmoxConfig::default()
        };
        assert_eq!(privileged.host_owner(), (1000, 1000));
    }

    #[test]
    fn api_error_message_reads_proxmox_errors() {
        assert_eq!(
            api_error_message(r#"{"data":null,"errors":{"vmid":"VM 100 already exists"}}"#),
            "vmid: VM 100 already exists"
        );
        assert_eq!(api_error_message("plain failure"), "plain failure");
    }
}
