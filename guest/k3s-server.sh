#!/bin/sh
# Userdata script for a k3s server (control plane) node.
#
# Usage:
#   husk run --name k3s-server --cpus 2 --memory 2048 \
#       --userdata guest/k3s-server.sh k3s-rootfs.ext4
#
# After boot, retrieve the join token:
#   husk exec k3s-server -- cat /var/lib/rancher/k3s/server/node-token

set -e

echo "[husk] Starting k3s server..."
k3s server --write-kubeconfig-mode=644 &

echo "[husk] Waiting for node to become Ready..."
timeout 120 sh -c '
    until k3s kubectl get node 2>/dev/null | grep -q " Ready"; do
        sleep 2
    done
'

echo "[husk] k3s server is ready."
k3s kubectl get node
