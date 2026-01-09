## agentman base image

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

## Configure versions

Edit `docker/versions.env`:
- **Base image**: `DEBIAN_TAG`
- **Tools**: `RUSTUP_VERSION`, `RUST_TOOLCHAIN`, `GO_VERSION`, `BUN_VERSION`, `UV_VERSION`, `PYTHON_VERSION`, `SDKMAN_VERSION`, `JAVA_VERSION`, `DUCKDB_VERSION`, `OPENCODE_VERSION`
- **User**: `USERNAME`, `USER_UID`, `USER_GID`

## Build (local)

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

## Remote SSH Access

The Docker image includes SSH server support for remote access. This is useful for connecting with editors like Zed.

The `agent` user has full root privileges via `sudo` (NOPASSWD) and is configured for SSH access.

### Starting the Container with SSH

To start the container with SSH enabled, set the `GITHUB_USERNAME` environment variable to automatically download and configure your SSH keys:

```bash
docker run -d \
  --name agentman \
  -p 2222:22 \
  -e GITHUB_USERNAME=your-github-username \
  agentman-base:dev
```

This will:
- Download your SSH public keys from `https://github.com/your-github-username.keys`
- Add them to `~/.ssh/authorized_keys` in the container
- Start the SSH server on port 22 (mapped to host port 2222)

### Connecting with Zed Editor

1. **Start the container** (as shown above):
   ```bash
   docker run -d \
     --name agentman \
     -p 2222:22 \
     -e GITHUB_USERNAME=your-github-username \
     agentman-base:dev
   ```

2. **Find the container's IP address**:
   ```bash
   docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' agentman
   ```
   Or if using host networking:
   ```bash
   docker run -d \
     --name agentman \
     --network host \
     -e GITHUB_USERNAME=your-github-username \
     agentman-base:dev
   ```

3. **In Zed Editor**:
   - Open the command palette (Cmd/Ctrl + Shift + P)
   - Select "Remote: Connect to Host..."
   - Enter: `agent@localhost:2222` (or use the container IP if not using port mapping)
   - Zed will connect using your SSH key

**Note**: Make sure your SSH public key is uploaded to your GitHub account (Settings → SSH and GPG keys) for the automatic key setup to work.

### Manual SSH Connection

You can also connect manually via SSH:

```bash
ssh -p 2222 agent@localhost
```

Or if using the container IP directly:

```bash
ssh agent@<container-ip>
```

## Build/push (CI / multi-arch)

Set `PLATFORMS` in `docker/versions.env` (example):

```bash
PLATFORMS=linux/amd64,linux/arm64
```

Then:

```bash
./scripts/push.sh
```

