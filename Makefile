SHELL := /bin/bash
.DEFAULT_GOAL := help

ENV_FILE ?= docker/versions.env

.PHONY: help
help:
	@echo "Targets:"
	@echo "  make vars        - print resolved build variables"
	@echo "  make build       - build image locally (single-platform, uses --load)"
	@echo "  make push        - build and push image (supports multi-platform)"
	@echo "  make build-nc    - build image without cache"
	@echo "  make run         - run opencode (default entrypoint)"
	@echo "  make shell       - start an interactive shell (override entrypoint)"

.PHONY: vars
vars:
	@set -a; . "$(ENV_FILE)"; set +a; \
	echo "ENV_FILE=$$(realpath "$(ENV_FILE)" 2>/dev/null || echo "$(ENV_FILE)")"; \
	env | grep -E '^(IMAGE_NAME|IMAGE_TAG|PLATFORMS|DEBIAN_TAG|RUSTUP_VERSION|RUST_TOOLCHAIN|GO_VERSION|BUN_VERSION|UV_VERSION|PYTHON_VERSION|OPENCODE_VERSION|USERNAME|USER_UID|USER_GID)=' | sort

.PHONY: build
build:
	@set -a; . "$(ENV_FILE)"; set +a; \
	docker buildx bake -f docker-bake.hcl --load

.PHONY: build-nc
build-nc:
	@set -a; . "$(ENV_FILE)"; set +a; \
	docker buildx bake -f docker-bake.hcl --load --no-cache

.PHONY: push
push:
	@set -a; . "$(ENV_FILE)"; set +a; \
	docker buildx bake -f docker-bake.hcl --push

.PHONY: run
run:
	@set -a; . "$(ENV_FILE)"; set +a; \
	docker run --rm -it "$${IMAGE_NAME}:$${IMAGE_TAG}"

.PHONY: shell
shell:
	@set -a; . "$(ENV_FILE)"; set +a; \
	docker run --rm -it --entrypoint bash "$${IMAGE_NAME}:$${IMAGE_TAG}"

