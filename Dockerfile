ARG DEBIAN_TAG=bookworm-slim
FROM debian:${DEBIAN_TAG}

# Keep this Dockerfile deterministic and CI-friendly:
# - All non-apt tools are installed with explicit, configurable versions.
# - Versions are set via build args (see `docker/versions.env` + `docker-bake.hcl`).

ARG DEBIAN_TAG

# Tool versions (override at build time)
ARG RUSTUP_VERSION=1.27.1
ARG RUST_TOOLCHAIN=1.92.0
ARG GO_VERSION=1.25.5
ARG BUN_VERSION=1.3.5
ARG UV_VERSION=0.9.22
ARG PYTHON_VERSION=3.13
ARG OPENCODE_VERSION=v1.1.7

# User config
ARG USERNAME=agent
ARG USER_UID=1000
ARG USER_GID=1000

ENV DEBIAN_FRONTEND=noninteractive \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      bash \
      ca-certificates \
      curl \
      wget \
      git \
      jq \
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
      lsof \
      nano \
      iproute2 \
      iputils-ping \
      net-tools \
      netcat-openbsd \
      psmisc \
      procps \
      ripgrep \
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
      python3-pip \
      python-is-python3 \
      pipx \
 && rm -rf /var/lib/apt/lists/*

# `bsdmainutils` may be absent on newer Debian releases; fall back to `bsdextrautils`.
RUN apt-get update \
 && if apt-get install -y --no-install-recommends bsdmainutils; then \
      true; \
    else \
      apt-get install -y --no-install-recommends bsdextrautils; \
    fi \
 && rm -rf /var/lib/apt/lists/*

SHELL ["/bin/bash", "-euxo", "pipefail", "-c"]

# Convenience: Debian provides `fdfind`, but most people expect `fd`.
RUN if command -v fdfind >/dev/null 2>&1 && ! command -v fd >/dev/null 2>&1; then ln -s "$(command -v fdfind)" /usr/local/bin/fd; fi

# --- Go (pinned) ---
RUN arch="$(dpkg --print-architecture)" \
 && case "${arch}" in \
      amd64) go_arch="amd64" ;; \
      arm64) go_arch="arm64" ;; \
      *) echo "Unsupported dpkg architecture: ${arch}" >&2; exit 1 ;; \
    esac \
 && curl -fsSL -o /tmp/go.tgz "https://go.dev/dl/go${GO_VERSION}.linux-${go_arch}.tar.gz" \
 && rm -rf /usr/local/go \
 && tar -C /usr/local -xzf /tmp/go.tgz \
 && rm -f /tmp/go.tgz \
 && ln -sf /usr/local/go/bin/go /usr/local/bin/go \
 && ln -sf /usr/local/go/bin/gofmt /usr/local/bin/gofmt \
 && go version

ENV PATH="/usr/local/go/bin:/usr/local/bun/bin:${PATH}"

# --- Bun (pinned) ---
ENV BUN_INSTALL=/usr/local/bun
RUN curl -fsSL https://bun.sh/install | bash -s "bun-v${BUN_VERSION}" \
 && ln -sf /usr/local/bun/bin/bun /usr/local/bin/bun \
 && bun --version

# --- uv (pinned) ---
# We download the exact release artifact for the platform and install `uv` + `uvx` to /usr/local/bin.
RUN arch="$(dpkg --print-architecture)" \
 && case "${arch}" in \
      amd64) uv_triple="x86_64-unknown-linux-gnu" ;; \
      arm64) uv_triple="aarch64-unknown-linux-gnu" ;; \
      *) echo "Unsupported dpkg architecture: ${arch}" >&2; exit 1 ;; \
    esac \
 && uv_archive="uv-${uv_triple}.tar.gz" \
 && curl -fsSL -o /tmp/uv.tar.gz "https://github.com/astral-sh/uv/releases/download/${UV_VERSION}/${uv_archive}" \
 && tar -C /tmp -xzf /tmp/uv.tar.gz \
 && install -m 0755 "/tmp/uv-${uv_triple}/uv" /usr/local/bin/uv \
 && install -m 0755 "/tmp/uv-${uv_triple}/uvx" /usr/local/bin/uvx \
 && rm -rf /tmp/uv.tar.gz "/tmp/uv-${uv_triple}" \
 && uv --version

# --- opencode (pinned) ---
RUN arch="$(dpkg --print-architecture)" \
 && case "${arch}" in \
      amd64) oc_arch="x64" ;; \
      arm64) oc_arch="arm64" ;; \
      *) echo "Unsupported dpkg architecture: ${arch}" >&2; exit 1 ;; \
    esac \
 && tmpdir="$(mktemp -d)" \
 && curl -fsSL -o /tmp/opencode.tar.gz "https://github.com/anomalyco/opencode/releases/download/${OPENCODE_VERSION}/opencode-linux-${oc_arch}.tar.gz" \
 && tar -C "${tmpdir}" -xzf /tmp/opencode.tar.gz \
 && install -m 0755 "${tmpdir}/opencode" /usr/local/bin/opencode \
 && rm -rf "${tmpdir}" /tmp/opencode.tar.gz \
 && opencode --version || true

# --- Non-root user ---
RUN groupadd --gid "${USER_GID}" "${USERNAME}" \
 && useradd --uid "${USER_UID}" --gid "${USER_GID}" --create-home --shell /bin/bash "${USERNAME}" \
 && echo "${USERNAME} ALL=(ALL) NOPASSWD:ALL" >"/etc/sudoers.d/${USERNAME}" \
 && chmod 0440 "/etc/sudoers.d/${USERNAME}" \
 && mkdir -p /workspace \
 && chown -R "${USERNAME}:${USERNAME}" /workspace

USER ${USERNAME}
WORKDIR /workspace

ENV HOME=/home/${USERNAME} \
    GOPATH=/home/${USERNAME}/go \
    PATH=/home/${USERNAME}/.cargo/bin:/home/${USERNAME}/.local/bin:/home/${USERNAME}/go/bin:/usr/local/go/bin:/usr/local/bun/bin:${PATH}

# --- Rust via rustup (pinned rustup-init + pinned toolchain) ---
RUN arch="$(dpkg --print-architecture)" \
 && case "${arch}" in \
      amd64) rustup_triple="x86_64-unknown-linux-gnu" ;; \
      arm64) rustup_triple="aarch64-unknown-linux-gnu" ;; \
      *) echo "Unsupported dpkg architecture: ${arch}" >&2; exit 1 ;; \
    esac \
 && curl -fsSL -o /tmp/rustup-init "https://github.com/rust-lang/rustup/releases/download/${RUSTUP_VERSION}/rustup-init-${rustup_triple}" \
 && chmod +x /tmp/rustup-init \
 && /tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain "${RUST_TOOLCHAIN}" \
 && rm -f /tmp/rustup-init \
 && rustc --version \
 && cargo --version \
 && rustup --version

# --- Python via uv (pinned) ---
# Per uv docs: `uv python install <version>` installs a Python version; `--default` also installs `python`/`python3`.
# https://docs.astral.sh/uv/guides/install-python/
RUN uv python install "${PYTHON_VERSION}" --default \
 && python --version \
 && python3 --version \
 && uv python list

# Set tini as entrypoint for proper signal handling.
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["opencode"]
