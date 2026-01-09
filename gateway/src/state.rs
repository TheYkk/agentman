//! Persistent state management for the gateway.
//!
//! Stores:
//! - SSH key fingerprint → GitHub username mappings
//! - (github_user, project) → container info mappings

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::RwLock;

/// Persistent gateway state.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GatewayState {
    /// Maps SSH key fingerprint (SHA256 base64) to GitHub username.
    #[serde(default)]
    pub key_to_github: HashMap<String, KeyCacheEntry>,

    /// Maps (github_user, project) to container info.
    /// Key format: "github_user/project"
    #[serde(default)]
    pub workspaces: HashMap<String, WorkspaceInfo>,
}

/// Cached key-to-GitHub mapping entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyCacheEntry {
    /// The GitHub username.
    pub github_username: String,

    /// When this mapping was verified.
    pub verified_at: DateTime<Utc>,

    /// The key type (e.g., "ssh-ed25519", "ssh-rsa").
    pub key_type: String,
}

/// Information about a workspace and its container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    /// GitHub username that owns this workspace.
    pub github_user: String,

    /// Project name.
    pub project: String,

    /// Docker container name.
    pub container_name: String,

    /// Docker container ID (if running).
    pub container_id: Option<String>,

    /// When the container was created.
    pub created_at: DateTime<Utc>,

    /// Path to the persistent workspace on the host.
    pub host_workspace_path: PathBuf,
}

impl WorkspaceInfo {
    /// Generate a workspace key for the hashmap.
    pub fn key(github_user: &str, project: &str) -> String {
        format!("{}/{}", github_user, project)
    }
}

/// Thread-safe state manager.
pub struct StateManager {
    state: RwLock<GatewayState>,
    path: PathBuf,
}

impl StateManager {
    /// Load state from disk, or create a new empty state.
    pub async fn load(path: PathBuf) -> Result<Self> {
        let state = if path.exists() {
            let content = tokio::fs::read_to_string(&path)
                .await
                .with_context(|| format!("Failed to read state file: {}", path.display()))?;
            serde_json::from_str(&content)
                .with_context(|| format!("Failed to parse state file: {}", path.display()))?
        } else {
            GatewayState::default()
        };

        Ok(Self {
            state: RwLock::new(state),
            path,
        })
    }

    /// Save state to disk.
    pub async fn save(&self) -> Result<()> {
        let state = self.state.read().await;
        let content = serde_json::to_string_pretty(&*state)
            .context("Failed to serialize state")?;

        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create state directory: {}", parent.display()))?;
        }

        tokio::fs::write(&self.path, content)
            .await
            .with_context(|| format!("Failed to write state file: {}", self.path.display()))?;

        Ok(())
    }

    /// Look up a GitHub username by SSH key fingerprint.
    pub async fn get_github_user(&self, fingerprint: &str) -> Option<KeyCacheEntry> {
        let state = self.state.read().await;
        state.key_to_github.get(fingerprint).cloned()
    }

    /// Cache a key-to-GitHub mapping.
    pub async fn cache_key(&self, fingerprint: String, entry: KeyCacheEntry) -> Result<()> {
        {
            let mut state = self.state.write().await;
            state.key_to_github.insert(fingerprint, entry);
        }
        self.save().await
    }

    /// Get workspace info by (github_user, project).
    pub async fn get_workspace(&self, github_user: &str, project: &str) -> Option<WorkspaceInfo> {
        let key = WorkspaceInfo::key(github_user, project);
        let state = self.state.read().await;
        state.workspaces.get(&key).cloned()
    }

    /// Save or update workspace info.
    pub async fn set_workspace(&self, info: WorkspaceInfo) -> Result<()> {
        let key = WorkspaceInfo::key(&info.github_user, &info.project);
        {
            let mut state = self.state.write().await;
            state.workspaces.insert(key, info);
        }
        self.save().await
    }

    /// Update container ID for an existing workspace.
    pub async fn update_container_id(
        &self,
        github_user: &str,
        project: &str,
        container_id: Option<String>,
    ) -> Result<()> {
        let key = WorkspaceInfo::key(github_user, project);
        {
            let mut state = self.state.write().await;
            if let Some(info) = state.workspaces.get_mut(&key) {
                info.container_id = container_id;
            }
        }
        self.save().await
    }

    /// List all workspaces for a given GitHub user.
    pub async fn list_workspaces(&self, github_user: &str) -> Vec<WorkspaceInfo> {
        let state = self.state.read().await;
        state
            .workspaces
            .values()
            .filter(|w| w.github_user == github_user)
            .cloned()
            .collect()
    }

    /// List all known GitHub users (from key cache).
    pub async fn list_github_users(&self) -> Vec<String> {
        let state = self.state.read().await;
        state
            .key_to_github
            .values()
            .map(|e| e.github_username.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect()
    }

    /// Remove a workspace mapping (and persist the state file).
    ///
    /// Returns the removed workspace info, if it existed.
    pub async fn remove_workspace(
        &self,
        github_user: &str,
        project: &str,
    ) -> Result<Option<WorkspaceInfo>> {
        let key = WorkspaceInfo::key(github_user, project);
        let removed = {
            let mut state = self.state.write().await;
            state.workspaces.remove(&key)
        };
        self.save().await?;
        Ok(removed)
    }
}
