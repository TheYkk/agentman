// CI-friendly build definition for `docker buildx bake`.
//
// You can override any `variable` via environment variables, e.g.:
//   PLATFORMS=linux/amd64,linux/arm64 docker buildx bake --push

variable "IMAGE_NAME"    { default = "agentman-base" }
variable "IMAGE_TAG"     { default = "dev" }
variable "PLATFORMS"     { default = "linux/amd64" }

variable "DEBIAN_TAG"    { default = "trixie-slim" }
variable "MISE_VERSION"     { default = "2026.10.22" }
variable "HERDR_VERSION"    { default = "0.9.5" }
variable "RUSTUP_VERSION"   { default = "1.29.2" }
variable "RUST_TOOLCHAIN"   { default = "1.99.1" }
variable "GO_VERSION"    { default = "1.27.3" }
variable "BUN_VERSION"   { default = "1.4.4" }
variable "NODE_VERSION"  { default = "22.23.4" }
variable "UV_VERSION"    { default = "0.13.6" }
variable "PYTHON_VERSION" { default = "3.13" }
variable "SDKMAN_VERSION" { default = "5.24.0" }
variable "JAVA_VERSION"   { default = "21.0.12-tem" }
variable "DUCKDB_VERSION" { default = "2.0.0" }
variable "OPENCODE_VERSION" { default = "2.2.0" }
variable "CLAUDE_CLI_VERSION"  { default = "2.1.312" }
variable "PI_VERSION"       { default = "1.4.0" }
variable "OMP_VERSION"      { default = "18.13.2" }
variable "CLOUDFLARED_VERSION" { default = "2026.10.5" }
variable "GH_VERSION"       { default = "2.103.3" }
variable "CADDY_VERSION"    { default = "2.11.7" }
variable "EZA_VERSION"      { default = "0.23.5" }
variable "BAT_VERSION"      { default = "0.26.1" }

variable "USERNAME"      { default = "agent" }
variable "USER_UID"      { default = "1000" }
variable "USER_GID"      { default = "1000" }

// Used by docker/metadata-action to inject tags/labels via an additional bake file.
// Local builds fall back to IMAGE_NAME:IMAGE_TAG here.
target "docker-metadata-action" {
  tags = ["${IMAGE_NAME}:${IMAGE_TAG}"]
}

group "default" {
  targets = ["agentman"]
}

target "agentman" {
  inherits = ["docker-metadata-action"]
  context    = "."
  dockerfile = "Dockerfile"

  platforms = [for p in split(",", PLATFORMS) : trimspace(p)]

  // Optional: lets mise use an authenticated GitHub API when downloading releases.
  secret = ["id=github_token,env=GITHUB_TOKEN"]

  args = {
    DEBIAN_TAG       = "${DEBIAN_TAG}"
    MISE_VERSION     = "${MISE_VERSION}"
    HERDR_VERSION    = "${HERDR_VERSION}"
    RUSTUP_VERSION   = "${RUSTUP_VERSION}"
    RUST_TOOLCHAIN   = "${RUST_TOOLCHAIN}"
    GO_VERSION       = "${GO_VERSION}"
    BUN_VERSION      = "${BUN_VERSION}"
    NODE_VERSION     = "${NODE_VERSION}"
    UV_VERSION       = "${UV_VERSION}"
    PYTHON_VERSION   = "${PYTHON_VERSION}"
    SDKMAN_VERSION   = "${SDKMAN_VERSION}"
    JAVA_VERSION     = "${JAVA_VERSION}"
    DUCKDB_VERSION   = "${DUCKDB_VERSION}"
    OPENCODE_VERSION = "${OPENCODE_VERSION}"
    CLAUDE_CLI_VERSION = "${CLAUDE_CLI_VERSION}"
    PI_VERSION       = "${PI_VERSION}"
    OMP_VERSION      = "${OMP_VERSION}"
    CLOUDFLARED_VERSION = "${CLOUDFLARED_VERSION}"
    GH_VERSION       = "${GH_VERSION}"
    CADDY_VERSION    = "${CADDY_VERSION}"
    EZA_VERSION      = "${EZA_VERSION}"
    BAT_VERSION      = "${BAT_VERSION}"

    USERNAME         = "${USERNAME}"
    USER_UID         = "${USER_UID}"
    USER_GID         = "${USER_GID}"
  }
}

