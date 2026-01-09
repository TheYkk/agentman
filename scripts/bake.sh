#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="${ENV_FILE:-${ROOT_DIR}/docker/versions.env}"

set -a
# shellcheck disable=SC1090
source "${ENV_FILE}"
set +a

exec docker buildx bake -f "${ROOT_DIR}/docker-bake.hcl" "$@"

