//! GitHub username resolution from SSH public keys.
//!
//! This module handles:
//! - Fetching a user's SSH public keys from `github.com/<user>.keys`
//! - Verifying a presented SSH key against a user's known keys
//! - Computing key fingerprints for caching

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use sha2::{Digest, Sha256};
use tracing::{debug, info};

/// HTTP client for fetching GitHub keys.
pub struct GitHubKeyFetcher {
    client: reqwest::Client,
}

impl GitHubKeyFetcher {
    /// Create a new GitHub key fetcher.
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent("agentman-gateway/0.1")
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to create HTTP client");
        Self { client }
    }

    /// Fetch SSH public keys for a GitHub user.
    ///
    /// Returns a list of key strings in OpenSSH format.
    pub async fn fetch_keys(&self, github_user: &str) -> Result<Vec<String>> {
        let url = format!("https://github.com/{}.keys", github_user);
        debug!("Fetching keys from {}", url);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("Failed to fetch keys for {}", github_user))?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "GitHub returned {} for user {}",
                response.status(),
                github_user
            ));
        }

        let body = response
            .text()
            .await
            .with_context(|| format!("Failed to read response for {}", github_user))?;

        let keys: Vec<String> = body
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        info!(
            "Fetched {} key(s) for GitHub user {}",
            keys.len(),
            github_user
        );

        Ok(keys)
    }

    /// Verify that a public key belongs to a GitHub user.
    ///
    /// Returns the key type (e.g., "ssh-ed25519") if the key is found.
    pub async fn verify_key(&self, github_user: &str, public_key: &str) -> Result<String> {
        let keys = self.fetch_keys(github_user).await?;

        // Normalize the presented key (remove comments, extra whitespace)
        let (presented_type, presented_data) = parse_ssh_key(public_key)?;
        let presented_normalized = format!("{} {}", presented_type, presented_data);

        for key in &keys {
            if let Ok((key_type, key_data)) = parse_ssh_key(key) {
                let key_normalized = format!("{} {}", key_type, key_data);
                if key_normalized == presented_normalized {
                    info!(
                        "Verified {} key for GitHub user {}",
                        presented_type, github_user
                    );
                    return Ok(presented_type);
                }
            }
        }

        Err(anyhow!(
            "Key not found in {}'s GitHub keys ({} keys checked)",
            github_user,
            keys.len()
        ))
    }
}

/// Parse an SSH public key string into (type, base64_data).
///
/// Handles formats like:
/// - "ssh-ed25519 AAAA... comment"
/// - "ssh-rsa AAAA... comment"
pub fn parse_ssh_key(key: &str) -> Result<(String, String)> {
    let parts: Vec<&str> = key.split_whitespace().collect();
    if parts.len() < 2 {
        return Err(anyhow!("Invalid SSH key format: too few parts"));
    }

    let key_type = parts[0].to_string();
    let key_data = parts[1].to_string();

    // Validate that key_data is valid base64
    base64::engine::general_purpose::STANDARD
        .decode(&key_data)
        .with_context(|| "Invalid base64 in SSH key")?;

    Ok((key_type, key_data))
}

/// Compute the SHA256 fingerprint of an SSH public key.
///
/// Returns the fingerprint in "SHA256:..." format used by `ssh-keygen -l`.
pub fn compute_fingerprint(public_key: &str) -> Result<String> {
    let (_, key_data) = parse_ssh_key(public_key)?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&key_data)
        .with_context(|| "Invalid base64 in SSH key")?;

    let mut hasher = Sha256::new();
    hasher.update(&decoded);
    let hash = hasher.finalize();

    // Format as SHA256:base64 (without trailing =)
    let b64 = base64::engine::general_purpose::STANDARD_NO_PAD.encode(hash);
    Ok(format!("SHA256:{}", b64))
}

/// Compute fingerprint from raw key bytes (wire format).
/// SSH fingerprint = SHA256(raw_key_bytes_in_wire_format)
pub fn compute_fingerprint_from_bytes(key_bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key_bytes);
    let hash = hasher.finalize();

    let b64 = base64::engine::general_purpose::STANDARD_NO_PAD.encode(hash);
    format!("SHA256:{}", b64)
}

/// Compute fingerprint from a russh public key.
/// The fingerprint is SHA256 of the raw key bytes in SSH wire format.
pub fn compute_fingerprint_from_pubkey(key: &russh::keys::PublicKey) -> String {
    use russh::keys::PublicKeyBase64;
    // public_key_bytes() returns the raw key data in SSH wire format
    let raw_bytes = key.public_key_bytes();
    compute_fingerprint_from_bytes(&raw_bytes)
}

/// Convert russh public key to OpenSSH string format for verification.
/// Returns format: "ssh-ed25519 AAAA..." or "ssh-rsa AAAA..."
pub fn public_key_to_openssh(key: &russh::keys::PublicKey) -> String {
    use russh::keys::PublicKeyBase64;
    
    // Get the key type string
    let key_type = match key.algorithm() {
        russh::keys::Algorithm::Ed25519 => "ssh-ed25519",
        russh::keys::Algorithm::Rsa { .. } => "ssh-rsa",
        russh::keys::Algorithm::Ecdsa { curve } => match curve {
            russh::keys::EcdsaCurve::NistP256 => "ecdsa-sha2-nistp256",
            russh::keys::EcdsaCurve::NistP384 => "ecdsa-sha2-nistp384",
            russh::keys::EcdsaCurve::NistP521 => "ecdsa-sha2-nistp521",
        },
        _ => "unknown",
    };
    
    // Get the base64-encoded key data
    let key_base64 = key.public_key_base64();
    
    format!("{} {}", key_type, key_base64)
}

/// Parse username from SSH username field.
///
/// Supports formats:
/// - "project" -> (project, None)
/// - "project+githubuser" -> (project, Some(githubuser))
pub fn parse_ssh_username(username: &str) -> (String, Option<String>) {
    if let Some(pos) = username.find('+') {
        let project = username[..pos].to_string();
        let github_user = username[pos + 1..].to_string();
        (project, Some(github_user))
    } else {
        (username.to_string(), None)
    }
}

/// Validate a project name (no path traversal, safe for container names).
pub fn validate_project_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(anyhow!("Project name cannot be empty"));
    }

    if name.len() > 64 {
        return Err(anyhow!("Project name too long (max 64 chars)"));
    }

    // Only allow alphanumeric, dash, underscore
    for c in name.chars() {
        if !c.is_alphanumeric() && c != '-' && c != '_' {
            return Err(anyhow!(
                "Invalid character '{}' in project name (only alphanumeric, dash, underscore allowed)",
                c
            ));
        }
    }

    // No leading dot or dash
    if name.starts_with('.') || name.starts_with('-') {
        return Err(anyhow!("Project name cannot start with '.' or '-'"));
    }

    Ok(())
}

/// Validate a GitHub username.
pub fn validate_github_username(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(anyhow!("GitHub username cannot be empty"));
    }

    if name.len() > 39 {
        return Err(anyhow!("GitHub username too long (max 39 chars)"));
    }

    // GitHub usernames: alphanumeric or single hyphens, cannot start/end with hyphen
    for c in name.chars() {
        if !c.is_alphanumeric() && c != '-' {
            return Err(anyhow!(
                "Invalid character '{}' in GitHub username",
                c
            ));
        }
    }

    if name.starts_with('-') || name.ends_with('-') {
        return Err(anyhow!("GitHub username cannot start or end with '-'"));
    }

    if name.contains("--") {
        return Err(anyhow!("GitHub username cannot contain consecutive hyphens"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ssh_username() {
        assert_eq!(
            parse_ssh_username("myproject"),
            ("myproject".to_string(), None)
        );
        assert_eq!(
            parse_ssh_username("myproject+octocat"),
            ("myproject".to_string(), Some("octocat".to_string()))
        );
        assert_eq!(
            parse_ssh_username("my-project+my-user"),
            ("my-project".to_string(), Some("my-user".to_string()))
        );
    }

    #[test]
    fn test_validate_project_name() {
        assert!(validate_project_name("myproject").is_ok());
        assert!(validate_project_name("my-project").is_ok());
        assert!(validate_project_name("my_project").is_ok());
        assert!(validate_project_name("MyProject123").is_ok());

        assert!(validate_project_name("").is_err());
        assert!(validate_project_name(".hidden").is_err());
        assert!(validate_project_name("-invalid").is_err());
        assert!(validate_project_name("path/traversal").is_err());
        assert!(validate_project_name("has spaces").is_err());
    }

    #[test]
    fn test_validate_github_username() {
        assert!(validate_github_username("octocat").is_ok());
        assert!(validate_github_username("my-user").is_ok());
        assert!(validate_github_username("User123").is_ok());

        assert!(validate_github_username("").is_err());
        assert!(validate_github_username("-invalid").is_err());
        assert!(validate_github_username("invalid-").is_err());
        assert!(validate_github_username("my--user").is_err());
        assert!(validate_github_username("has spaces").is_err());
    }

    #[test]
    fn test_parse_ssh_key() {
        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl test@example.com";
        let (key_type, _key_data) = parse_ssh_key(key).unwrap();
        assert_eq!(key_type, "ssh-ed25519");
    }
}
