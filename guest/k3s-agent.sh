#!/bin/sh
# Userdata script for a k3s agent (worker) node.
#
# Requires K3S_URL and K3S_TOKEN environment variables, passed via:
#   husk run --name k3s-agent-1 --cpus 2 --memory 2048 \
#       --userdata guest/k3s-agent.sh \
#       -e "K3S_URL=https://172.20.0.2:6443" \
#       -e "K3S_TOKEN=$(husk exec k3s-server -- cat /var/lib/rancher/k3s/server/node-token)" \
#       k3s-rootfs.ext4

set -e

if [ -z "$K3S_URL" ] || [ -z "$K3S_TOKEN" ]; then
    echo "[husk] Error: K3S_URL and K3S_TOKEN must be set"
    echo "  Pass via: husk run -e K3S_URL=... -e K3S_TOKEN=..."
    exit 1
fi

echo "[husk] Joining k3s cluster at $K3S_URL..."
k3s agent --server="$K3S_URL" --token="$K3S_TOKEN" &

echo "[husk] k3s agent started (joining cluster in background)."
