#!/usr/bin/env bash
set -euo pipefail

# Script to download SSH keys from GitHub and add them to authorized_keys
# Reads GITHUB_USERNAME from environment variable

if [ -z "${GITHUB_USERNAME:-}" ]; then
    echo "Error: GITHUB_USERNAME environment variable is not set" >&2
    exit 1
fi

SSH_DIR="${HOME}/.ssh"
AUTHORIZED_KEYS="${SSH_DIR}/authorized_keys"

# Create .ssh directory if it doesn't exist
mkdir -p "${SSH_DIR}"
chmod 700 "${SSH_DIR}"

# Download SSH keys from GitHub
echo "Downloading SSH keys for GitHub user: ${GITHUB_USERNAME}"
KEYS_URL="https://github.com/${GITHUB_USERNAME}.keys"

# Download keys and append to authorized_keys
if curl -fsSL "${KEYS_URL}" >> "${AUTHORIZED_KEYS}" 2>/dev/null; then
    # Ensure proper permissions
    chmod 600 "${AUTHORIZED_KEYS}"
    echo "Successfully added SSH keys from GitHub to ${AUTHORIZED_KEYS}"
else
    echo "Warning: Failed to download SSH keys from ${KEYS_URL}" >&2
    exit 1
fi
