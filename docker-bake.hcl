// CI-friendly build definition for docker buildx bake.
//
// Usage (local):
//   set -a; . docker/versions.env; set +a
//   docker buildx bake --load
//
// Usage (CI, multi-arch):
//   set -a; . docker/versions.env; set +a
//   docker buildx bake --push

variable "IMAGE_NAME"    { default = "agentman-base" }
variable "IMAGE_TAG"     { default = "dev" }
variable "PLATFORMS"     { default = "linux/amd64" }

variable "DEBIAN_TAG"    { default = "bookworm-slim" }
variable "RUSTUP_VERSION"   { default = "1.27.1" }
variable "RUST_TOOLCHAIN"   { default = "1.92.0" }
variable "GO_VERSION"    { default = "1.25.5" }
variable "BUN_VERSION"   { default = "1.3.5" }
variable "UV_VERSION"    { default = "0.9.22" }
variable "PYTHON_VERSION" { default = "3.13" }
variable "DUCKDB_VERSION" { default = "1.4.3" }
variable "OPENCODE_VERSION" { default = "v1.1.7" }

variable "USERNAME"      { default = "agent" }
variable "USER_UID"      { default = "1000" }
variable "USER_GID"      { default = "1000" }

group "default" {
  targets = ["agentman"]
}

target "agentman" {
  context    = "."
  dockerfile = "Dockerfile"

  tags = ["${IMAGE_NAME}:${IMAGE_TAG}"]
  platforms = [for p in split(",", PLATFORMS) : trimspace(p)]

  args = {
    DEBIAN_TAG       = "${DEBIAN_TAG}"
    RUSTUP_VERSION   = "${RUSTUP_VERSION}"
    RUST_TOOLCHAIN   = "${RUST_TOOLCHAIN}"
    GO_VERSION       = "${GO_VERSION}"
    BUN_VERSION      = "${BUN_VERSION}"
    UV_VERSION       = "${UV_VERSION}"
    PYTHON_VERSION   = "${PYTHON_VERSION}"
    DUCKDB_VERSION   = "${DUCKDB_VERSION}"
    OPENCODE_VERSION = "${OPENCODE_VERSION}"

    USERNAME         = "${USERNAME}"
    USER_UID         = "${USER_UID}"
    USER_GID         = "${USER_GID}"
  }
}

