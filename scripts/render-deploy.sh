#!/usr/bin/env bash
# Render deploy/template for one standalone domain.
# Usage: scripts/render-deploy.sh <domain> [output-dir]
#
# Embedded domains are refused: their primary runs inside a host process, and a
# rendered StatefulSet would be a second primary on its own volume (E2/E4,
# noetl/ehgraph#10). A domain is embedded when it is listed below or its catalog
# resource says spec.deployment.mode: embedded.
set -euo pipefail
EMBEDDED_DOMAINS="noetl-cluster"
domain="${1:?domain required, for example network}"
root="$(cd "$(dirname "$0")/.." && pwd)"
resource="$root/playbooks/automation/agents/ehgraph/resources/ehgraph-${domain}.yaml"
for embedded in $EMBEDDED_DOMAINS; do
  if [[ "$domain" == "$embedded" ]]; then
    echo "refusing: ${domain} is embedded in its host (E4); it has no standalone StatefulSet. See deploy/instances/ehgraph-${domain}/README.md" >&2
    exit 2
  fi
done
if [[ -f "$resource" ]] && grep -Eq '^[[:space:]]+mode:[[:space:]]*embedded[[:space:]]*$' "$resource"; then
  echo "refusing: ${resource#"$root"/} says mode: embedded; ${domain} has no standalone StatefulSet" >&2
  exit 2
fi
out="${2:-deploy/instances/ehgraph-${domain}}"
mkdir -p "$out"
sed "s/__DOMAIN__/${domain}/g" "$root/deploy/template/statefulset.yaml" > "$out/statefulset.yaml"
if ! grep -q "replicas: 1" "$out/statefulset.yaml"; then
  echo "rendered manifest is missing replicas: 1" >&2
  exit 1
fi
echo "wrote $out/statefulset.yaml"
