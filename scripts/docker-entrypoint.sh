#!/usr/bin/env bash
set -euo pipefail

# Entrypoint script that sets up SSH and runs the original command

# Copy files from /pre_workspace to /workspace if they don't already exist
if [ -d /pre_workspace ]; then
    for file in /pre_workspace/*; do
        if [ -f "$file" ]; then
            filename=$(basename "$file")
            if [ ! -f "/workspace/$filename" ]; then
                cp "$file" "/workspace/$filename"
                echo "Copied $filename to /workspace"
            fi
        fi
    done
fi

# Setup SSH keys from GitHub if GITHUB_USERNAME is set
if [ -n "${GITHUB_USERNAME:-}" ]; then
    /usr/local/bin/setup-ssh-keys.sh || {
        echo "Warning: Failed to setup SSH keys, continuing anyway..." >&2
    }
fi

# Start SSH server in the background
if [ -f /etc/ssh/sshd_config ]; then
    # Ensure SSH directory exists and has correct permissions
    mkdir -p ~/.ssh
    chmod 700 ~/.ssh
    if [ -f ~/.ssh/authorized_keys ]; then
        chmod 600 ~/.ssh/authorized_keys
    fi
    
    # Start SSH daemon
    sudo /usr/sbin/sshd -D &
    SSH_PID=$!
    echo "SSH server started (PID: $SSH_PID)"
    # Give SSH server a moment to start
    sleep 1
fi

# Execute the original command
exec "$@"
