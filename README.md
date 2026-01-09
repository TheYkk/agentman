## agentman

Debian-based base image for a coding agent with pinned versions of:
- **Rust** (via `rustup`) + pinned toolchain
- **rust-analyzer** (via `rustup component add`, pinned to toolchain)
- **Go**
- **Bun**
- **uv** + **Python** installed via `uv python install`
- **Java** (via SDKMAN, pinned)
- **DuckDB** (CLI)
- **opencode** (default command under `tini`)

Python is installed using uv per the official guide: `https://docs.astral.sh/uv/guides/install-python/`.

`opencode` refers to the OpenCode project: `https://github.com/anomalyco/opencode`.

## SSH Gateway

The **agentman-gateway** is a Rust SSH server that provides streamlined access to agent containers. It automatically:

- Authenticates users via SSH public keys (verified against GitHub)
- Creates/manages Docker containers per project
- Persists `/workspace` across sessions
- Supports port forwarding for development workflows

### Quick Start

1. **Start the gateway** on your server:
   ```bash
   # Generate default config
   agentman-gateway --generate-config > /etc/agentman/gateway.toml

   # Run the gateway
   agentman-gateway -c /etc/agentman/gateway.toml
   ```

2. **Connect from your laptop**:
   ```bash
   # First time: provide your GitHub username
   ssh myproject+octocat@agent-server

   # After first auth, just use the project name
   ssh myproject@agent-server
   ```
  Tip: interactive sessions (PTY) attach to `tmux` by default — detach with `Ctrl-b d`, then reconnect to resume. Non-interactive SSH commands (editor bootstrap) are not wrapped in tmux.

3. **Add to ~/.ssh/config** for convenience:
   ```
   Host agent
     HostName agent-server.example.com
     Port 2222
     User myproject+octocat
   ```

   Then simply:
   ```bash
   ssh agent
   ```

### How It Works

```
┌─────────────┐     SSH      ┌──────────────────┐     Docker     ┌─────────────────┐
│  Your       │ ──────────▶  │  agentman-       │ ─────────────▶ │  Agent          │
│  Laptop     │              │  gateway         │                │  Container      │
└─────────────┘              └──────────────────┘                └─────────────────┘
                                    │                                    │
                                    ▼                                    ▼
                             ┌──────────────┐                    ┌───────────────┐
                             │ State Cache  │                    │  /workspace   │
                             │ (key→github) │                    │  (persistent) │
                             └──────────────┘                    └───────────────┘
```

1. **SSH Connection**: You connect to the gateway using `ssh project@gateway`
2. **Key Verification**: Gateway checks your SSH public key against GitHub's API
3. **Container Provisioning**: Creates/starts a container named `project-github-YYYYMMDD`
4. **Workspace Persistence**: Bind-mounts `/var/lib/agentman/workspaces/<github>/<project>` to `/workspace`
5. **Session**: Your interactive shell runs inside the container via Docker exec (attached to a persistent `tmux` session by default when a PTY is requested)

### Authentication Flow

The gateway supports two authentication modes:

**Non-interactive (for editors like Zed/VS Code)**:
```bash
ssh myproject+octocat@gateway
```
The `+octocat` tells the gateway your GitHub username. It verifies your SSH key is in `github.com/octocat.keys` and caches the mapping.

**Interactive (first time from terminal)**:
```bash
ssh myproject@gateway
# Gateway prompts: "GitHub username: "
# You enter: octocat
# Gateway verifies and caches
```

After the first successful auth, the key→GitHub mapping is cached, so you can just use `ssh myproject@gateway`.

### Port Forwarding

**Local forwarding (`-L`)** — Access container services from your laptop:
```bash
# Forward local:8080 to container:3000
ssh -L 8080:localhost:3000 myproject@gateway
```

**Remote forwarding (`-R`)** — Expose local services to the container:
```bash
# Make localhost:9000 accessible as host.docker.internal:9000 inside the container
ssh -R 9000:localhost:9000 myproject@gateway
```

This is useful for:
- Running a dev server locally and accessing it from the container
- Exposing your local language server to remote code
- Sharing a local database with the container

### Editor Integration

**Zed Editor**:
1. Add to `~/.ssh/config`:
   ```
   Host agent
     HostName your-server.com
     Port 2222
     User myproject+yourgithub
   ```
2. In Zed: `Cmd+Shift+P` → "Remote: Connect to Host..." → `agent`

**VS Code Remote-SSH**:
1. Add same config to `~/.ssh/config`
2. Open Command Palette → "Remote-SSH: Connect to Host..." → `agent`

**Cursor**:
Works the same as VS Code Remote-SSH.

### Configuration

Generate default config:
```bash
agentman-gateway --generate-config
```

Example `/etc/agentman/gateway.toml`:
```toml
listen_addr = "0.0.0.0:2222"
docker_image = "agentman-base:dev"
workspace_root = "/var/lib/agentman/workspaces"
state_file = "/var/lib/agentman/state.json"
host_key_path = "/var/lib/agentman/host_key"

# Pre-authorized GitHub users (auto-matched on first connect)
bootstrap_github_users = ["octocat", "defunkt"]

[shell]
mode = "tmux"
tmux_session = "agentman"

[port_forwarding]
allow_local = true      # Allow -L (local port forward)
allow_remote = true     # Allow -R (remote port forward)
allow_gateway_ports = false  # Bind -R only to loopback
allow_nonlocal_destinations = false  # Only forward to localhost/container

[container_security]
cap_drop_all = true
cap_add = ["CHOWN", "DAC_OVERRIDE", "FOWNER", "SETGID", "SETUID"]
no_new_privileges = true
readonly_rootfs = false
memory_limit = "4g"
cpu_limit = 2.0
use_seccomp = true
```

### Container Security

Containers are created with security hardening by default:

- **No privileged mode**: Containers never run privileged
- **No Docker socket**: Container cannot escape to host via Docker API
- **Capability dropping**: All caps dropped, minimal set re-added
- **No-new-privileges**: Prevents privilege escalation via setuid binaries
- **Seccomp**: Default Docker seccomp profile applied
- **Resource limits**: Memory and CPU limits enforced
- **Isolated networking**: Bridge network only, no host network

The `/workspace` bind-mount is the only host path exposed to containers.

### Container Naming

Containers follow the pattern: `{project}-{github}-{YYYYMMDD}`

Example: `myproject-octocat-20260109`

If multiple containers exist for the same project/user/date, a suffix is added: `myproject-octocat-20260109-1`

### Workspace Persistence

Each `(github_user, project)` pair gets a persistent workspace directory:
```
/var/lib/agentman/workspaces/
└── octocat/
    ├── myproject/
    │   └── (your code here, persisted across container restarts)
    └── another-project/
        └── ...
```

The workspace is bind-mounted to `/workspace` inside the container. This persists across:
- Container restarts
- New container creation (e.g., after image updates)
- Gateway restarts

---

## Base Image

### Configure versions

Edit `docker/versions.env`:
- **Base image**: `DEBIAN_TAG`
- **Tools**: `RUSTUP_VERSION`, `RUST_TOOLCHAIN`, `GO_VERSION`, `BUN_VERSION`, `UV_VERSION`, `PYTHON_VERSION`, `SDKMAN_VERSION`, `JAVA_VERSION`, `DUCKDB_VERSION`, `OPENCODE_VERSION`
- **User**: `USERNAME`, `USER_UID`, `USER_GID`

### Build (local)

Requires Docker BuildKit + buildx.

```bash
./scripts/build.sh
```

Run the image (defaults to running `opencode` under `tini`):

```bash
docker run --rm -it agentman-base:dev
```

To pass flags / arguments to opencode:

```bash
docker run --rm -it agentman-base:dev opencode --help
```

Open a shell instead:

```bash
docker run --rm -it --entrypoint bash agentman-base:dev
```

### Build/push (CI / multi-arch)

Set `PLATFORMS` in `docker/versions.env` (example):

```bash
PLATFORMS=linux/amd64,linux/arm64
```

Then:

```bash
./scripts/push.sh
```

---

## Legacy: Direct SSH Access

The Docker image also includes a built-in SSH server for direct container access (without the gateway).

To start a container with SSH enabled:

```bash
docker run -d \
  --name agentman \
  -p 2222:22 \
  -e GITHUB_USERNAME=your-github-username \
  agentman-base:dev
```

Connect:
```bash
ssh -p 2222 agent@localhost
```

**Note**: This legacy mode doesn't provide the gateway's features (automatic container management, persistence, port forwarding to container). Use the gateway for production workflows.
