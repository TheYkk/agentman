# syntax=docker/dockerfile:1
ARG DEBIAN_TAG=trixie-slim
FROM debian:${DEBIAN_TAG}

# Keep this Dockerfile deterministic and CI-friendly:
# - All non-apt tools are installed with explicit, configurable versions.
# - Versions are set via build args (see `docker-bake.hcl`).
# - Most tools are installed with mise (https://mise.jdx.dev), which resolves versions via
#   https://mise-versions.jdx.dev; Rust (rustup), Python (uv) and Java (SDKMAN) keep their own installers.

ARG DEBIAN_TAG

# Tool versions (override at build time)
ARG MISE_VERSION=2026.10.9
ARG HERDR_VERSION=0.9.4
ARG RUSTUP_VERSION=1.27.1
ARG RUST_TOOLCHAIN=1.92.0
ARG GO_VERSION=1.27.2
ARG BUN_VERSION=1.4.3
ARG NODE_VERSION=22.23.3
ARG UV_VERSION=0.13.0
ARG PYTHON_VERSION=3.13
ARG SDKMAN_VERSION=5.20.0
ARG JAVA_VERSION=21.0.9-tem
ARG DUCKDB_VERSION=1.5.6
ARG OPENCODE_VERSION=2.0.28
ARG CLAUDE_CLI_VERSION=2.1.296
ARG PI_VERSION=1.2.0
ARG OMP_VERSION=18.10.3
ARG CLOUDFLARED_VERSION=2026.10.0
ARG GH_VERSION=2.102.0
ARG CADDY_VERSION=2.11.7
ARG EZA_VERSION=0.23.5
ARG BAT_VERSION=0.26.1

# User config
ARG USERNAME=agent
ARG USER_UID=1000
ARG USER_GID=1000

ENV DEBIAN_FRONTEND=noninteractive \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8 \
    SHELL=/bin/bash

# `bsdmainutils` may be absent on newer Debian releases; fall back to `bsdextrautils`.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      bash \
      ca-certificates \
      curl \
      wget \
      git \
      jq \
      grep \
      gawk \
      sed \
      findutils \
      coreutils \
      openssh-client \
      openssh-server \
      sudo \
      tini \
      tzdata \
      \
      # C/C++ toolchain
      build-essential \
      make \
      cmake \
      file \
      pkg-config \
      libssl-dev \
      \
      # Common dev utilities
      htop \
      less \
      linux-perf \
      lsof \
      nano \
      iproute2 \
      iputils-ping \
      net-tools \
      netcat-openbsd \
      psmisc \
      procps \
      ripgrep \
      tmux \
      fd-find \
      rsync \
      socat \
      sqlite3 \
      tree \
      unzip \
      util-linux \
      vim \
      xz-utils \
      zip \
      docker.io \
      docker-cli \
      docker-compose \
      postgresql-client \
      default-mysql-client \
      python3 \
      python3-pip \
      python3-venv \
      python-is-python3 \
      pipx \
 && if apt-get install -y --no-install-recommends bsdmainutils; then \
      true; \
    else \
      apt-get install -y --no-install-recommends bsdextrautils; \
    fi \
 && rm -rf /var/lib/apt/lists/*

SHELL ["/bin/bash", "-euxo", "pipefail", "-c"]

# Convenience: Debian provides `fdfind`, but most people expect `fd`.
RUN if command -v fdfind >/dev/null 2>&1 && ! command -v fd >/dev/null 2>&1; then ln -s "$(command -v fdfind)" /usr/local/bin/fd; fi

# --- mise (pinned) ---
RUN arch="$(dpkg --print-architecture)" \
 && case "${arch}" in \
      amd64) mise_arch="x64" ;; \
      arm64) mise_arch="arm64" ;; \
      *) echo "Unsupported dpkg architecture: ${arch}" >&2; exit 1 ;; \
    esac \
 && curl -fsSL -o /tmp/mise "https://github.com/jdx/mise/releases/download/v${MISE_VERSION}/mise-v${MISE_VERSION}-linux-${mise_arch}" \
 && install -m 0755 /tmp/mise /usr/local/bin/mise \
 && rm -f /tmp/mise \
 && mise --version

# Agents are pinned by mise, so turn off Claude Code's self-updater.
ENV DISABLE_AUTOUPDATER=1

# --- SSH Server Setup (as root) ---
RUN mkdir -p /var/run/sshd \
 && sed -i 's/#PermitRootLogin prohibit-password/PermitRootLogin no/' /etc/ssh/sshd_config \
 && sed -i 's/#PasswordAuthentication yes/PasswordAuthentication no/' /etc/ssh/sshd_config \
 && sed -i 's/#PubkeyAuthentication yes/PubkeyAuthentication yes/' /etc/ssh/sshd_config \
 && echo "AllowUsers ${USERNAME}" >> /etc/ssh/sshd_config \
 && echo "Port 22" >> /etc/ssh/sshd_config

# --- Non-root user with root privileges ---
RUN groupadd --gid "${USER_GID}" "${USERNAME}" \
 && useradd --uid "${USER_UID}" --gid "${USER_GID}" --create-home --shell /bin/bash "${USERNAME}" \
 && usermod -aG sudo "${USERNAME}" \
 && echo "${USERNAME} ALL=(ALL) NOPASSWD:ALL" >"/etc/sudoers.d/${USERNAME}" \
 && chmod 0440 "/etc/sudoers.d/${USERNAME}" \
 && mkdir -p /workspace \
 && chown -R "${USERNAME}:${USERNAME}" /workspace \
 && mkdir -p /home/${USERNAME}/.ssh \
 && chmod 700 /home/${USERNAME}/.ssh \
 && chown -R "${USERNAME}:${USERNAME}" /home/${USERNAME}/.ssh

# Login shells (e.g. `bash -l` in tmux) reset PATH in /etc/profile; add the tool dirs back.
# Tool locations are pinned via ENV below, so they do not depend on $HOME
# (the gateway sets HOME=/workspace for non-PTY SSH commands).
RUN printf '%s\n' \
      '# agentman: tool paths (mise shims, cargo, go, sdkman)' \
      'for p in /home/${USERNAME}/.sdkman/candidates/java/current/bin /home/${USERNAME}/go/bin /home/${USERNAME}/.local/share/mise/shims /home/${USERNAME}/.local/bin /home/${USERNAME}/.cargo/bin; do' \
      '  case ":${PATH}:" in *":${p}:"*) ;; *) PATH="${p}:${PATH}" ;; esac' \
      'done' \
      'export PATH' \
    | sed "s|\${USERNAME}|${USERNAME}|g" >/etc/profile.d/agentman-path.sh \
 && chmod 0644 /etc/profile.d/agentman-path.sh

USER ${USERNAME}
WORKDIR /workspace
ENV HOME=/home/${USERNAME} \
    GOPATH=/home/${USERNAME}/go \
    SDKMAN_DIR=/home/${USERNAME}/.sdkman \
    JAVA_HOME=/home/${USERNAME}/.sdkman/candidates/java/current \
    MISE_YES=1 \
    MISE_DATA_DIR=/home/${USERNAME}/.local/share/mise \
    MISE_CONFIG_DIR=/home/${USERNAME}/.config/mise \
    MISE_CACHE_DIR=/home/${USERNAME}/.cache/mise \
    MISE_STATE_DIR=/home/${USERNAME}/.local/state/mise \
    RUSTUP_HOME=/home/${USERNAME}/.rustup \
    CARGO_HOME=/home/${USERNAME}/.cargo \
    PATH=/home/${USERNAME}/.cargo/bin:/home/${USERNAME}/.local/bin:/home/${USERNAME}/.local/share/mise/shims:/home/${USERNAME}/go/bin:/home/${USERNAME}/.sdkman/candidates/java/current/bin:${PATH}

# --- Tools via mise (pinned) ---
# Installed into the agent's global mise config (~/.config/mise/config.toml), exposed through mise shims.
# An optional `github_token` build secret raises GitHub API rate limits for release downloads.
RUN --mount=type=secret,id=github_token,env=GITHUB_TOKEN \
    mise use -g \
      "herdr@${HERDR_VERSION}" \
      "cloudflared@${CLOUDFLARED_VERSION}" \
      "gh@${GH_VERSION}" \
      "caddy@${CADDY_VERSION}" \
      "duckdb@${DUCKDB_VERSION}" \
      "eza@${EZA_VERSION}" \
      "bat@${BAT_VERSION}" \
      "go@${GO_VERSION}" \
      "node@${NODE_VERSION}" \
      "bun@${BUN_VERSION}" \
      "uv@${UV_VERSION}" \
 && herdr --version \
 && cloudflared --version \
 && gh --version \
 && caddy version \
 && duckdb --version \
 && eza --version \
 && bat --version \
 && go version \
 && node --version \
 && npm --version \
 && bun --version \
 && uv --version

# Coding agents (npm packages need Node.js from the step above; omp runs on Bun).
# opencode and Claude Code fetch their native binary in a postinstall script, which mise skips unless
# it shells out to npm and the tool opts in to install scripts.
RUN --mount=type=secret,id=github_token,env=GITHUB_TOKEN \
    mise settings set npm.shell_out true \
 && mise use -g \
      "npm:@opencode/cli[npm_args=--ignore-scripts=false]@${OPENCODE_VERSION}" \
      "npm:@anthropic-ai/claude-code[npm_args=--ignore-scripts=false]@${CLAUDE_CLI_VERSION}" \
      "npm:@earendil-works/pi-coding-agent@${PI_VERSION}" \
      "npm:@oh-my-pi/pi-coding-agent@${OMP_VERSION}" \
 && opencode --version \
 && claude --version \
 && pi --version \
 && omp --version

# --- Rust via rustup (pinned rustup-init + pinned toolchain) ---
RUN arch="$(dpkg --print-architecture)" \
 && case "${arch}" in \
      amd64) rustup_triple="x86_64-unknown-linux-gnu" ;; \
      arm64) rustup_triple="aarch64-unknown-linux-gnu" ;; \
      *) echo "Unsupported dpkg architecture: ${arch}" >&2; exit 1 ;; \
    esac \
 && curl -fsSL -o /tmp/rustup-init "https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/${rustup_triple}/rustup-init" \
 && chmod +x /tmp/rustup-init \
 && /tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain "${RUST_TOOLCHAIN}" \
 && rm -f /tmp/rustup-init \
 && rustup component add rust-analyzer \
 && rustc --version \
 && cargo --version \
 && rust-analyzer --version \
 && rustup --version

# --- Python via uv (pinned) ---
# Per uv docs: `uv python install <version>` installs a Python version; `--default` also installs `python`/`python3`.
# https://docs.astral.sh/uv/guides/install-python/
RUN uv python install "${PYTHON_VERSION}" --default \
 && python --version \
 && python3 --version \
 && uv python list

# --- Java via SDKMAN (pinned) ---
# Install SDKMAN CLI from a pinned GitHub release, then install a pinned Java distribution.
RUN arch="$(dpkg --print-architecture)" \
 && case "${arch}" in \
      amd64) sdk_platform="linuxx64" ;; \
      arm64) sdk_platform="linuxarm64" ;; \
      *) echo "Unsupported dpkg architecture: ${arch}" >&2; exit 1 ;; \
    esac \
 && tmpdir="$(mktemp -d)" \
 && curl -fsSL -o "${tmpdir}/sdkman.zip" "https://github.com/sdkman/sdkman-cli/releases/download/${SDKMAN_VERSION}/sdkman-cli-${SDKMAN_VERSION}.zip" \
 && unzip -q "${tmpdir}/sdkman.zip" -d "${tmpdir}" \
 && mkdir -p "${SDKMAN_DIR}" \
 && cp -a "${tmpdir}/sdkman-${SDKMAN_VERSION}/." "${SDKMAN_DIR}/" \
 && rm -rf "${tmpdir}" \
 && mkdir -p "${SDKMAN_DIR}/etc" "${SDKMAN_DIR}/var" "${SDKMAN_DIR}/candidates" "${SDKMAN_DIR}/tmp" "${SDKMAN_DIR}/ext" \
 && printf '%s\n' \
      '# SDKMAN configuration' \
      'sdkman_auto_answer=true' \
      'sdkman_auto_complete=false' \
      'sdkman_auto_env=false' \
    >"${SDKMAN_DIR}/etc/config" \
 && printf '%s' "${sdk_platform}" >"${SDKMAN_DIR}/var/platform" \
 && printf '%s' 'java' >"${SDKMAN_DIR}/var/candidates" \
 && export SDKMAN_CANDIDATES_API="https://api.sdkman.io/2" \
 && export SDKMAN_BROKER_API="https://broker.sdkman.io" \
 && set +u \
 && source "${SDKMAN_DIR}/bin/sdkman-init.sh" \
 && sdk version \
 && sdk install java "${JAVA_VERSION}" \
 && java -version \
 && set -u

# Some SDKMAN commands expect a version file at `${SDKMAN_DIR}/var/version`.
RUN mkdir -p "${SDKMAN_DIR}/var" \
 && printf '%s' "${SDKMAN_VERSION}" >"${SDKMAN_DIR}/var/version"

# Ensure interactive shells have SDKMAN available.
RUN printf '\n# SDKMAN\nexport SDKMAN_DIR=\"$HOME/.sdkman\"\n[[ -s \"$SDKMAN_DIR/bin/sdkman-init.sh\" ]] && source \"$SDKMAN_DIR/bin/sdkman-init.sh\"\n' \
 | tee -a "${HOME}/.bashrc" "${HOME}/.profile" >/dev/null

# --- herdr agent integrations ---
# Lets herdr track agent state (working, blocked, done) for each installed agent.
RUN mkdir -p "${HOME}/.config/opencode" "${HOME}/.claude" "${HOME}/.pi/agent/extensions" "${HOME}/.omp/agent/extensions" \
 && for agent in opencode claude pi omp; do herdr integration install "${agent}"; done

# Copy scripts (switch to root temporarily for installation)
USER root
COPY scripts/setup-ssh-keys.sh /usr/local/bin/setup-ssh-keys.sh
COPY scripts/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
COPY scripts/herdr-default-agent.sh /usr/local/bin/herdr-default-agent.sh
RUN chmod +x /usr/local/bin/setup-ssh-keys.sh /usr/local/bin/docker-entrypoint.sh /usr/local/bin/herdr-default-agent.sh \
 && chown root:root /usr/local/bin/setup-ssh-keys.sh /usr/local/bin/docker-entrypoint.sh /usr/local/bin/herdr-default-agent.sh

# Create pre_workspace folder for files that should be copied to /workspace on container start
RUN mkdir -p /pre_workspace \
 && chown -R ${USERNAME}:${USERNAME} /pre_workspace
COPY AGENTS.md /pre_workspace/
RUN chown -R ${USERNAME}:${USERNAME} /pre_workspace

USER ${USERNAME}

# Set tini as entrypoint for proper signal handling.
# Use custom entrypoint that sets up SSH
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/docker-entrypoint.sh"]

# herdr's headless server is the container's main process; attach with `herdr` (e.g. over SSH).
# On start, the entrypoint opens a workspace in /workspace running AGENTMAN_DEFAULT_AGENT (empty disables it).
ENV AGENTMAN_DEFAULT_AGENT=opencode
CMD ["herdr", "server"]
