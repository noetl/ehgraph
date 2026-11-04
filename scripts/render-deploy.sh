#!/usr/bin/env bash
# Render deploy/template for one domain.
# Usage: scripts/render-deploy.sh <domain> [output-dir]
set -euo pipefail
domain="${1:?domain required, for example network or noetl-cluster}"
out="${2:-deploy/instances/ehgraph-${domain}}"
root="$(cd "$(dirname "$0")/.." && pwd)"
mkdir -p "$out"
sed "s/__DOMAIN__/${domain}/g" "$root/deploy/template/statefulset.yaml" > "$out/statefulset.yaml"
if ! grep -q "replicas: 1" "$out/statefulset.yaml"; then
  echo "rendered manifest is missing replicas: 1" >&2
  exit 1
fi
echo "wrote $out/statefulset.yaml"
