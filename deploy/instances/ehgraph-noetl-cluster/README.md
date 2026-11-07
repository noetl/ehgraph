# ehgraph-noetl-cluster: embedded, no manifest here

`noetl-cluster` is embedded in noetl-server (owner decision E4, 2026-11-06). Its one
primary runs inside the noetl-server process and is served at `/ehgraph/noetl-cluster`
on the server's own Service:

    http://noetl.noetl.svc.cluster.local:8082/ehgraph/noetl-cluster/mcp

There is deliberately **no StatefulSet for this domain**. A standalone
`ehgraph-noetl-cluster` pod would open a second primary for the same domain on its own
volume. Neither the data-directory lock (it is per volume) nor the embedded host's
process-wide domain claim (it is per process) can see a primary in another pod, so the
two would silently diverge. Keeping the manifest out of the repo is the guard:

- `scripts/render-deploy.sh noetl-cluster` refuses.
- The `deploy`, `undeploy`, `redeploy`, and `restart` lifecycle playbooks refuse any
  resource with `spec.deployment.mode: embedded`, and any embedded domain by name.
- The catalog resource `playbooks/automation/agents/ehgraph/resources/ehgraph-noetl-cluster.yaml`
  has no mutating lifecycle verbs. Rolling or restarting this graph means rolling
  noetl-server, which is a server operation, not an ehgraph one.
- `crates/ehgraph/tests/deploy_manifests.rs` fails CI if a manifest for an embedded
  domain reappears under `deploy/instances/`, or if the resource and the guards drift.

The embedded instance is off until noetl-server is started with
`NOETL_EHGRAPH_EMBEDDED=true`, `NOETL_EHGRAPH_HOST_REPLICAS=1`, and an ehgraph config
at `NOETL_EHGRAPH_CONFIG` (see the wiki page Embedding). If a standalone
`ehgraph-noetl-cluster` was ever applied to a cluster, delete its StatefulSet and Service
before arming the embedded one. Keep or archive its PVC by hand; nothing reads it after
that.

To run `noetl-cluster` standalone again, the decision E4 has to change first. Then
restore the mutating lifecycle verbs on the resource, drop `mode: embedded`, remove the
domain from `EMBEDDED_DOMAINS` in `scripts/render-deploy.sh` and the lifecycle
playbooks, and render the manifest.
