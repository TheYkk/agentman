## agentman

Debian-based base image for a coding agent with pinned versions of the tools below. Most are installed with [mise](https://mise.jdx.dev) (versions resolved via `https://mise-versions.jdx.dev`); Rust uses rustup, Python uses uv, and Java uses SDKMAN.
- **herdr** (terminal workspace manager for coding agents; its headless server is the default command under `tini`)
- **Rust** (via `rustup`) + pinned toolchain
- **rust-analyzer** (via `rustup component add`, pinned to toolchain)
- **Go**
- **Bun**
- **Node.js**
- **uv** + **Python** installed via `uv python install`
- **Java** (via SDKMAN, pinned)
- **DuckDB** (CLI)
- **opencode** v2 (default agent: opened in a herdr workspace in `/workspace` on start)
- **Claude Code**, **pi** and **omp** (oh-my-pi) coding agents, with herdr integrations installed for all four
- **cloudflared** (Cloudflare Tunnel client), **Caddy** and the GitHub CLI (`gh`)
- CLI basics: `docker`, `git`, `jq`, `rg`, `fd`, `bat`, `eza`, `sqlite3`

Python is installed using uv per the official guide: `https://docs.astral.sh/uv/guides/install-python/`.

`herdr` refers to `https://github.com/herdrdev/herdr`; `opencode` refers to the OpenCode project: `https://github.com/anomalyco/opencode`.

## SSH Gateway

The **agentman-gateway** is a Rust SSH server that provides streamlined access to agent containers. It automatically:

- Authenticates users via SSH public keys (verified against GitHub)
- Creates/manages a container per project (Docker, or LXC on Proxmox VE; see [Running on Proxmox VE](#running-on-proxmox-ve-lxc))
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

Note: editor integrations typically run non-PTY SSH exec probes (e.g. `ssh -T host "cd; uname -sm"`). The gateway does **not** wrap these in tmux and returns a proper SSH exit status so editors can bootstrap reliably.

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

### SSH Agent Forwarding (ForwardAgent)

If you enable SSH agent forwarding on your client, the gateway can expose your local SSH agent inside the container as `SSH_AUTH_SOCK`. This lets you use your laptop’s keys for things like GitHub SSH without copying private keys into the sandbox.

Add to your `~/.ssh/config`:

```
Host agent
  ForwardAgent yes
```

Then inside the container:

```bash
echo "$SSH_AUTH_SOCK"
ssh-add -L
```

Security note: any process inside the container can ask your forwarded agent to sign during the lifetime of the SSH connection. Enable only if you trust the remote environment.

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

[agent_forwarding]
allow = true  # Allow ForwardAgent / SSH_AUTH_SOCK inside the container

[container_security]
cap_drop_all = true
cap_add = ["CHOWN", "DAC_OVERRIDE", "FOWNER", "SETGID", "SETUID"]
no_new_privileges = true
readonly_rootfs = false
# Optional resource limits (omit for no limits; defaults to no limits)
# memory_limit = "4g"
# cpu_limit = 2.0
use_seccomp = true
```

### Container Security

Containers are created with security hardening by default:

- **No privileged mode**: Containers never run privileged (unless you opt into `docker_in_sandbox.mode = "privileged"`)
- **No Docker socket**: Container cannot escape to host via Docker API
- **Capability dropping**: All caps dropped, minimal set re-added
- **No-new-privileges**: Prevents privilege escalation via setuid binaries
- **Seccomp**: Default Docker seccomp profile applied
- **Optional resource limits**: Memory/CPU limits are configurable (default: no limits)
- **Isolated networking**: Bridge network only, no host network

The `/workspace` bind-mount is the only host path exposed to containers.

### Running on Proxmox VE (LXC)

Instead of Docker, the gateway can run each sandbox as an **LXC container on Proxmox VE**. Set `runtime = "proxmox"` and fill in the `[proxmox]` section. Everything else (SSH auth, tmux sessions, port/agent forwarding, `agentman list/stop/pause/stats/destroy`) works the same.

How it works:
- The gateway runs **as root on the Proxmox node** that hosts the containers.
- Lifecycle (create, start, shutdown, destroy, status, stats) goes through the **Proxmox VE API** with an API token.
- The workspace is bind-mounted at `/workspace` and feature flags are set with `pct set <vmid> --mp0 ... --features ...` (Proxmox only lets `root@pam` add bind mounts or change features other than `nesting`, which API tokens cannot do).
- Shells and commands are attached with `lxc-attach` and drop to UID/GID 1000 (`exec_uid`/`exec_gid`) with `setpriv --init-groups`, so the user keeps groups like `docker`. Interactive sessions get a real PTY and follow window resizes.
- Docker inside the sandbox follows `[docker_in_sandbox] mode`: unless it is `off` (and by default, since LXC containers are unprivileged with nesting), containers get `features: nesting=1,keyctl=1` and the gateway starts `dockerd` as container root with `lxc-attach` on create and on every reconnect. Inner Docker data lives in the container's root filesystem, so it survives reconnects and restarts and is removed with `agentman destroy`.
- `agentman pause` freezes the container with `lxc-freeze`; reconnecting unfreezes it.
- Containers are unprivileged by default, so the gateway chowns workspaces to the shifted host UID (`id_map_offset + exec_uid`, i.e. `101000`).

Setup:

1. **Template.**
   - *Proxmox VE 8.x (and 9.x):* build a regular container template from the image. It adds systemd as init, a DHCP client, an `agentman.service` that runs the image's entrypoint, and saves the image's ENV to `/etc/agentman/image.env`, which the gateway applies to every session (templates without it get only `[proxmox] exec_env`):
     ```bash
     scripts/build-lxc-template.sh ghcr.io/theykk/agentman-base:latest agentman-base.tar.zst   # needs docker, jq, zstd
     scp agentman-base.tar.zst root@pve:/var/lib/vz/template/cache/
     ```
     and set `ostemplate = "local:vztmpl/agentman-base.tar.zst"`. If the node rejects the template's Debian version, set `ostype = "unmanaged"`; the template then configures `eth0` with DHCP itself.
   - *Proxmox VE 9.1+:* you can instead pull the image as an OCI template (Datacenter → Storage → CT Templates → *Pull from OCI Registry*, e.g. `ghcr.io/theykk/agentman-base:latest`) and set `ostemplate` to the resulting volume. A plain `docker export` without an init does not boot on any version.
   - Any other LXC template works as long as it has a user with UID 1000 and the tools you need (`bash`, `tmux`, `socat` for `ssh -L`, `setpriv`, and `dockerd` for Docker inside).
2. **API token.** Give a dedicated user exactly what container management needs, scoped to a resource pool. New containers are created *into* the pool, so the token never needs rights on individual `/vms/<id>` paths and any free VMID works:
   ```bash
   pveum pool add agentman
   pveum role add AgentmanGateway --privs "VM.Allocate VM.Audit VM.Config.CPU VM.Config.Disk VM.Config.Memory VM.Config.Network VM.Config.Options VM.PowerMgmt Datastore.AllocateSpace Datastore.Audit SDN.Use Sys.Audit"
   pveum user add agentman@pve
   pveum acl modify /pool/agentman                 --users agentman@pve --roles AgentmanGateway
   pveum acl modify /storage/local-lvm             --users agentman@pve --roles AgentmanGateway  # rootfs storage
   pveum acl modify /storage/local                 --users agentman@pve --roles AgentmanGateway  # template storage
   pveum acl modify /sdn/zones/localnetwork/vmbr0  --users agentman@pve --roles AgentmanGateway  # bridge for net0
   pveum acl modify /nodes/$(hostname -s)          --users agentman@pve --roles AgentmanGateway  # task status
   pveum user token add agentman@pve gateway --privsep 0   # prints the secret
   install -m 600 /dev/stdin /etc/agentman/proxmox-token <<< 'SECRET'
   ```
   Set `pool = "agentman"` in `[proxmox]`: without it, Proxmox checks `VM.Allocate` on `/vms/<new id>` and a pool-scoped token cannot create containers. (The simpler alternative is one ACL on `/` with `PVEVMAdmin,PVEDatastoreUser,PVESDNUser`, which then covers every VM on the cluster.) Bind mounts, feature flags other than `nesting`, and tags are applied with `pct` as root on the node, because Proxmox refuses them for API tokens (or, for tags, checks them against `/vms/<id>` before the container is in the pool).
3. **Config** (`/etc/agentman/gateway.toml`):
   ```toml
   runtime = "proxmox"

   [proxmox]
   api_url = "https://127.0.0.1:8006"
   token_id = "agentman@pve!gateway"
   token_secret_file = "/etc/agentman/proxmox-token"  # or AGENTMAN_PROXMOX_TOKEN_SECRET
   ca_cert = "/etc/pve/pve-root-ca.pem"              # the cluster CA every node has
   ostemplate = "local:vztmpl/agentman-base.tar.zst"
   pool = "agentman"
   storage = "local-lvm"
   rootfs_size_gb = 16
   bridge = "vmbr0"
   ip = "dhcp"
   ```
   If the node's API certificate is expired or not signed by `ca_cert`, the gateway says so in its error; renew it with `pvecm updatecerts --force` (then `systemctl restart pveproxy`). `insecure_skip_tls_verify = true` is for testing only.
   `container_security.memory_limit` and `cpu_limit` map to the container's memory and cores/cpulimit; without a memory limit, `default_memory_mb` (4096) is used.
4. **Service.** Use `gateway/examples/agentman-gateway-proxmox.service` (it runs as root without the Docker-specific sandboxing, since `pct` and `lxc-attach` need it).

Limitations: `ssh -R` listeners bind on the Proxmox host, and there is no `host.docker.internal` alias inside LXC containers. Docker's capability/seccomp settings in `[container_security]` do not apply; LXC containers use Proxmox's AppArmor profile and user namespaces instead. Workspaces created under one runtime are not reused by the other; switching runtimes provisions a fresh container on the same workspace directory.

### Docker Inside Sandboxes

Each sandbox can run its own Docker daemon, so agents can `docker build`, `docker run` and `docker compose up` without touching the host's Docker socket. One `[docker_in_sandbox]` section controls this for every sandbox backend:

```toml
[docker_in_sandbox]
mode = "auto"                  # auto | on | off
docker_isolation = "sysbox"    # Docker backend only: sysbox | privileged
sysbox_runtime = "sysbox-runc" # Docker backend only
persist_data = true            # Docker backend only: keep inner images across container recreation
```

- `auto` (default) enables it only where it needs no extra privileges, and otherwise leaves sandboxes as they are. On the Docker backend that means the host has [sysbox](https://github.com/nestybox/sysbox) installed (it registers a `sysbox-runc` runtime); sysbox runs the sandbox in a user namespace, so the inner `dockerd` gets the capabilities it needs while sandbox root stays unprivileged on the host.
- `on` always enables it. On the Docker backend it uses `docker_isolation`: `sysbox` makes the gateway refuse to start without the runtime, and `privileged` runs sandboxes with `--privileged` for hosts that cannot install sysbox. Root in a privileged sandbox is root on the host, so only use it for fully trusted users.
- `off` disables it.

When enabled, the gateway starts `dockerd` as root inside the sandbox whenever it is created or reconnected to (logs in `/var/log/dockerd.log`), with the socket owned by the `agent` user's group so `docker` works without `sudo`. Nothing extra is needed in the image beyond the `docker.io` package it already ships.

On the Docker backend, inner Docker data lives in a named volume `agentman-docker-<github_user>-<project>`, which `agentman destroy` deletes together with the workspace (`--keep-workspace` keeps both). The mode is recorded on each container, so a change only applies to sandboxes created afterwards; destroy a sandbox (with `--keep-workspace`) to switch it.

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

**Permissions note (important for Zed/VS Code Remote SSH):** the gateway bind-mounts a host directory into `/workspace`. The container runs as a non-root user (UID/GID **1000** by default), so the host workspace directory must be writable by that user. The gateway will attempt to `chown`/`chmod` the workspace directory automatically; if you run the gateway without permission to do that, fix it on the host (or set `workspace_root` to a location with correct ownership).

### Destroying a Sandbox (Kill + Delete Persistent Workspace)

The gateway supports a small set of **control commands** via SSH exec. This lets you stop/remove your sandbox container and optionally delete the persistent workspace directory on the host.

Delete the container(s) **and** the persisted workspace data:
```bash
ssh myproject@gateway agentman destroy --yes
```

Stop/remove the container(s) but **keep** the persisted workspace data:
```bash
ssh myproject@gateway agentman destroy --keep-workspace
```

Preview what would be deleted:
```bash
ssh myproject@gateway agentman destroy --dry-run
```

### Sandbox Control (List / Stop / Pause / Stats)

List all sandboxes for your GitHub user:
```bash
ssh myproject@gateway agentman list
```

Stop the **current** sandbox container (keeps the persisted workspace data on disk):
```bash
ssh myproject@gateway agentman stop
```

Pause the **current** sandbox container:
```bash
ssh myproject@gateway agentman pause
```

Show CPU/memory and **persisted workspace storage** stats for **all** your sandboxes:
```bash
ssh myproject@gateway agentman stats
```

Show stats only for the **current** sandbox:
```bash
ssh myproject@gateway agentman stats --current
```

Watch stats (refresh every second) for **all** your sandboxes:
```bash
ssh myproject@gateway agentman stats --watch
```

Watch stats only for the **current** sandbox:
```bash
ssh myproject@gateway agentman stats --current --watch
```

Note: `agentman exec <cmd>` is accepted as an alias (e.g. `agentman exec stats --current`).

---

## Base Image

### Configure versions

Edit `docker-bake.hcl` (the `variable` defaults near the top), or override via environment variables:
- **Base image**: `DEBIAN_TAG`
- **Tools**: `MISE_VERSION`, `HERDR_VERSION`, `RUSTUP_VERSION`, `RUST_TOOLCHAIN`, `GO_VERSION`, `BUN_VERSION`, `NODE_VERSION`, `UV_VERSION`, `PYTHON_VERSION`, `SDKMAN_VERSION`, `JAVA_VERSION`, `DUCKDB_VERSION`, `OPENCODE_VERSION`, `CLAUDE_CLI_VERSION`, `PI_VERSION`, `OMP_VERSION`, `CLOUDFLARED_VERSION`, `GH_VERSION`, `CADDY_VERSION`, `EZA_VERSION`, `BAT_VERSION`
- **User**: `USERNAME`, `USER_UID`, `USER_GID`

### Build (local)

Requires Docker BuildKit + buildx.

```bash
./scripts/build.sh
```

Run the image (defaults to running `herdr server` under `tini`):

```bash
docker run -d --name agentman agentman-base:dev
```

On start, opencode is opened in a herdr workspace in `/workspace`. Pick another agent with `-e AGENTMAN_DEFAULT_AGENT=claude` (or `pi`, `omp`), or set it to an empty value to start with no agent.

Attach to the herdr session (inside the container, or over SSH):

```bash
docker exec -it agentman herdr
```

Run a different command instead (for example opencode):

```bash
docker run --rm -it agentman-base:dev opencode --help
```

Open a shell instead:

```bash
docker run --rm -it --entrypoint bash agentman-base:dev
```

### Build/push (CI / multi-arch)

Set `PLATFORMS` (example):

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
