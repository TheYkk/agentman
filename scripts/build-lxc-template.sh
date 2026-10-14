#!/usr/bin/env bash
# Build a Proxmox VE LXC template (rootfs tarball) from the agentman Docker image.
#
# Proxmox VE 9.1+ can use the OCI image directly. Older releases (8.x) boot a container template
# with /sbin/init, which the Docker image does not have, so this script layers on:
#   - systemd as init, plus ifupdown and a DHCP client so the container gets its network,
#   - an `agentman.service` unit that runs the image's ENTRYPOINT + CMD as the image user,
#     with the image's environment,
# then exports the filesystem as a .tar.zst that Proxmox accepts as a template.
#
# Usage:
#   scripts/build-lxc-template.sh [IMAGE] [OUTPUT]
#   IMAGE  defaults to ghcr.io/theykk/agentman-base:latest
#   OUTPUT defaults to agentman-base.tar.zst
#
# Then copy OUTPUT to the node's template cache (e.g. /var/lib/vz/template/cache/) and set
#   [proxmox] ostemplate = "local:vztmpl/agentman-base.tar.zst"
#
# Requires: docker, jq, zstd.
set -euo pipefail

IMAGE="${1:-ghcr.io/theykk/agentman-base:latest}"
OUTPUT="${2:-agentman-base.tar.zst}"

for tool in docker jq zstd; do
    command -v "$tool" >/dev/null 2>&1 || { echo "error: $tool is required" >&2; exit 1; }
done

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    docker pull "$IMAGE"
fi

ctx="$(mktemp -d)"
cid=""
cleanup() {
    [ -n "$cid" ] && docker rm -f "$cid" >/dev/null 2>&1 || true
    rm -rf "$ctx"
}
trap cleanup EXIT

config="$(docker image inspect --format '{{json .Config}}' "$IMAGE")"
user="$(jq -r '.User // ""' <<<"$config")"
user="${user:-root}"
workdir="$(jq -r '.WorkingDir // ""' <<<"$config")"
workdir="${workdir:-/}"

# systemd ExecStart= takes a command line; quote every word.
exec_start="$(jq -r '[(.Entrypoint // [])[], (.Cmd // [])[]] | map("\"" + gsub("\""; "\\\"") + "\"") | join(" ")' <<<"$config")"
if [ -z "$exec_start" ]; then
    echo "error: $IMAGE has no ENTRYPOINT or CMD" >&2
    exit 1
fi

# The image's ENV, as a systemd EnvironmentFile.
jq -r '(.Env // [])[]' <<<"$config" >"$ctx/image.env"

cat >"$ctx/agentman.service" <<EOF
[Unit]
Description=agentman sandbox entrypoint (from $IMAGE)
After=network-online.target
Wants=network-online.target

[Service]
User=$user
WorkingDirectory=$workdir
EnvironmentFile=/etc/agentman/image.env
# tini is not PID 1 under systemd; let it reap children as a subreaper instead.
Environment=TINI_SUBREAPER=1
ExecStart=$exec_start
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
EOF

# Fallback network config. Proxmox rewrites this for managed OS types (honoring the
# container's net0 settings); with ostype=unmanaged this DHCP config is used as-is.
cat >"$ctx/interfaces" <<'EOF'
auto lo
iface lo inet loopback

auto eth0
iface eth0 inet dhcp
EOF

cat >"$ctx/Dockerfile" <<EOF
FROM $IMAGE
USER root
RUN apt-get update \\
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \\
      systemd systemd-sysv dbus ifupdown iproute2 \\
 && (DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends isc-dhcp-client \\
     || DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends dhcpcd-base) \\
 && rm -rf /var/lib/apt/lists/*
COPY image.env /etc/agentman/image.env
COPY agentman.service /etc/systemd/system/agentman.service
COPY interfaces /etc/network/interfaces
# The gateway starts dockerd itself (as root via lxc-attach, with the socket group it needs);
# docker.io's own units would race it and leave the socket unusable.
RUN systemctl enable agentman.service networking.service \\
 && systemctl mask systemd-networkd-wait-online.service docker.service docker.socket \\
 && : > /etc/machine-id
EOF

tag="agentman-lxc-template:build-$$"
docker build --pull=false -t "$tag" "$ctx"
cid="$(docker create "$tag")"
docker export "$cid" | zstd -T0 -3 -q -o "$OUTPUT" -f
docker rmi "$tag" >/dev/null 2>&1 || true

echo "Wrote $OUTPUT"
echo "Copy it to /var/lib/vz/template/cache/ on the Proxmox node and set:"
echo "  [proxmox] ostemplate = \"local:vztmpl/$(basename "$OUTPUT")\""
