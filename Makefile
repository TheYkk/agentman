SHELL := /bin/bash
.DEFAULT_GOAL := help

ENV_FILE ?= docker/versions.env

.PHONY: help
help:
	@echo "Targets:"
	@echo ""
	@echo "  Docker Image:"
	@echo "    make vars        - print resolved build variables"
	@echo "    make build       - build image locally (single-platform, uses --load)"
	@echo "    make push        - build and push image (supports multi-platform)"
	@echo "    make build-nc    - build image without cache"
	@echo "    make run         - run opencode (default entrypoint)"
	@echo "    make shell       - start an interactive shell (override entrypoint)"
	@echo ""
	@echo "  Gateway:"
	@echo "    make gateway         - build the SSH gateway (release)"
	@echo "    make gateway-debug   - build the SSH gateway (debug)"
	@echo "    make gateway-run     - run the gateway locally"
	@echo "    make gateway-install - install gateway to /usr/local/bin"

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

# Gateway targets
.PHONY: gateway
gateway:
	cd gateway && cargo build --release

.PHONY: gateway-debug
gateway-debug:
	cd gateway && cargo build

.PHONY: gateway-run
gateway-run:
	cd gateway && cargo run -- --generate-config > /tmp/gateway.toml || true
	cd gateway && cargo run -- -c /tmp/gateway.toml -v

.PHONY: gateway-install
gateway-install: gateway
	sudo install -m 755 gateway/target/release/agentman-gateway /usr/local/bin/
	sudo mkdir -p /etc/agentman
	@if [ ! -f /etc/agentman/gateway.toml ]; then \
		sudo cp gateway/examples/gateway.toml /etc/agentman/; \
		echo "Installed example config to /etc/agentman/gateway.toml"; \
	else \
		echo "Config already exists at /etc/agentman/gateway.toml"; \
	fi

.PHONY: gateway-clean
gateway-clean:
	cd gateway && cargo clean
