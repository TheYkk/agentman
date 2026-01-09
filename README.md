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

## Build/push (CI / multi-arch)

Set `PLATFORMS` in `docker/versions.env` (example):

```bash
PLATFORMS=linux/amd64,linux/arm64
```

Then:

```bash
./scripts/push.sh
```

