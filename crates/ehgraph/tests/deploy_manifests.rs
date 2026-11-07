//! Deploy manifests and catalog resources agree with the embedded/standalone split
//! (decisions E2/E4, noetl/ehgraph#10).
//!
//! The failure this guards: `noetl-cluster` is embedded in noetl-server, but a
//! standalone StatefulSet for it stayed in the repo, and its `kind: Mcp` resource still
//! pointed at that standalone Service with a `deploy` verb. Applying either would open
//! a second primary for the domain on its own volume. The data-dir lock is per volume
//! and the embedded domain claim is per process, so nothing would refuse it.
//!
//! The resources under `playbooks/automation/agents/ehgraph/resources/` are the source
//! of truth: a resource with `spec.deployment.mode: embedded` is an embedded domain.
//! Everything else here is checked against them.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// noetl-server's in-cluster Service (the gateway and workers use the same URL).
const HOST_BASE: &str = "http://noetl.noetl.svc.cluster.local:8082";
const MUTATING_VERBS: [&str; 4] = ["deploy", "undeploy", "redeploy", "restart"];
const READ_VERBS: [&str; 2] = ["status", "discover"];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn read(rel: &str) -> String {
    fs::read_to_string(root().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// The value of the first `key: value` line, ignoring indentation. The resources are
/// flat and hand-written, and no key used here repeats within one file.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}:");
    text.lines().find_map(|line| {
        let rest = line.trim_start().strip_prefix(&prefix)?;
        let value = rest.trim();
        (!value.is_empty()).then_some(value)
    })
}

fn has_key(text: &str, key: &str) -> bool {
    let prefix = format!("{key}:");
    text.lines().any(|l| l.trim_start().starts_with(&prefix))
}

struct Resource {
    file: String,
    domain: String,
    text: String,
}

impl Resource {
    fn embedded(&self) -> bool {
        field(&self.text, "mode") == Some("embedded")
    }
    fn get(&self, key: &str) -> &str {
        field(&self.text, key).unwrap_or_else(|| panic!("{} has no `{key}`", self.file))
    }
}

fn resources() -> Vec<Resource> {
    let dir = root().join("playbooks/automation/agents/ehgraph/resources");
    let mut out: Vec<Resource> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "yaml"))
        .map(|p| {
            let text = fs::read_to_string(&p).unwrap();
            let file = p.file_name().unwrap().to_string_lossy().into_owned();
            let path = field(&text, "path").unwrap_or_else(|| panic!("{file}: no path"));
            let domain = path.rsplit('/').next().unwrap().to_string();
            Resource { file, domain, text }
        })
        .collect();
    out.sort_by(|a, b| a.domain.cmp(&b.domain));
    assert!(
        out.len() >= 2,
        "expected the network and noetl-cluster resources"
    );
    out
}

fn embedded_domains() -> BTreeSet<String> {
    resources()
        .into_iter()
        .filter(Resource::embedded)
        .map(|r| r.domain)
        .collect()
}

/// `EMBEDDED_DOMAINS="a b"` as written in a script or playbook.
fn declared_embedded(text: &str, file: &str) -> BTreeSet<String> {
    let line = text
        .lines()
        .find_map(|l| l.trim_start().strip_prefix("EMBEDDED_DOMAINS="))
        .unwrap_or_else(|| panic!("{file} has no EMBEDDED_DOMAINS guard"));
    line.trim_matches('"')
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

fn instance_manifest(domain: &str) -> PathBuf {
    root().join(format!(
        "deploy/instances/ehgraph-{domain}/statefulset.yaml"
    ))
}

#[test]
fn resources_are_consistent_with_their_names() {
    for r in resources() {
        assert_eq!(r.get("name"), format!("ehgraph-{}", r.domain), "{}", r.file);
        assert_eq!(r.get("domain"), r.domain, "{}", r.file);
        assert_eq!(r.file, format!("ehgraph-{}.yaml", r.domain));
    }
    assert!(
        embedded_domains().contains("noetl-cluster"),
        "E4: noetl-cluster is embedded in noetl-server"
    );
    assert!(
        !embedded_domains().contains("network"),
        "E4: network stays standalone"
    );
}

#[test]
fn an_embedded_domain_points_at_its_host_mount_and_cannot_be_deployed() {
    assert!(
        !embedded_domains().is_empty(),
        "no embedded resource to check"
    );
    for r in resources().into_iter().filter(Resource::embedded) {
        let d = &r.domain;
        let mount = format!("/ehgraph/{d}");
        let base = format!("{HOST_BASE}{mount}");
        assert_eq!(r.get("host"), "noetl-server", "{}", r.file);
        assert_eq!(r.get("mount"), mount, "{}", r.file);
        assert_eq!(r.get("url"), format!("{base}/mcp"), "{}", r.file);
        assert_eq!(r.get("initialize_url"), format!("{base}/healthz"));
        assert_eq!(r.get("tools_list_url"), format!("{base}/mcp/tools"));
        assert!(
            !r.text.contains(".ehgraph.svc"),
            "{} still names a standalone ehgraph Service",
            r.file
        );
        for verb in MUTATING_VERBS {
            assert!(
                !has_key(&r.text, verb),
                "{} has a `{verb}` verb; an embedded graph is rolled with its host",
                r.file
            );
        }
        for verb in READ_VERBS {
            assert!(has_key(&r.text, verb), "{} lost `{verb}`", r.file);
        }
        for key in ["release_name", "storage", "service_port", "namespace"] {
            assert!(!has_key(&r.text, key), "{} has standalone `{key}`", r.file);
        }
    }
}

#[test]
fn no_standalone_manifest_exists_for_an_embedded_domain() {
    assert!(
        !embedded_domains().is_empty(),
        "no embedded resource to check"
    );
    for d in embedded_domains() {
        let dir = root().join(format!("deploy/instances/ehgraph-{d}"));
        let yaml: Vec<_> = fs::read_dir(&dir)
            .map(|it| {
                it.map(|e| e.unwrap().path())
                    .filter(|p| p.extension().is_some_and(|x| x == "yaml" || x == "yml"))
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            yaml.is_empty(),
            "{d} is embedded but has Kubernetes manifests {yaml:?}: a second primary"
        );
    }
}

#[test]
fn every_standalone_domain_has_its_rendered_single_replica_manifest() {
    let standalone: Vec<_> = resources().into_iter().filter(|r| !r.embedded()).collect();
    assert!(!standalone.is_empty());
    for r in &standalone {
        let d = &r.domain;
        assert_eq!(
            r.get("url"),
            format!("http://ehgraph-{d}.ehgraph.svc.cluster.local:8080/mcp"),
            "{}",
            r.file
        );
        for verb in MUTATING_VERBS.iter().chain(&READ_VERBS) {
            assert!(has_key(&r.text, verb), "{} lost `{verb}`", r.file);
        }
        let committed = fs::read_to_string(instance_manifest(d))
            .unwrap_or_else(|e| panic!("standalone {d} has no manifest: {e}"));
        assert!(
            committed.contains("  replicas: 1\n"),
            "{d}: replicas must be 1"
        );

        // The committed copy is exactly what the render script produces now.
        let out = tempfile::tempdir().unwrap();
        let status = render(d, out.path());
        assert!(status.success(), "render {d} failed");
        let rendered = fs::read_to_string(out.path().join("statefulset.yaml")).unwrap();
        assert_eq!(
            rendered, committed,
            "deploy/instances/ehgraph-{d} drifted from the template; rerun scripts/render-deploy.sh {d}"
        );
    }

    // And nothing under deploy/instances is unaccounted for.
    let known: BTreeSet<_> = standalone
        .iter()
        .map(|r| format!("ehgraph-{}", r.domain))
        .collect();
    for entry in fs::read_dir(root().join("deploy/instances")).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.join("statefulset.yaml").exists() {
            assert!(
                known.contains(&name),
                "{name} has a StatefulSet but no standalone catalog resource"
            );
        }
    }
}

fn render(domain: &str, out: &Path) -> std::process::ExitStatus {
    Command::new("bash")
        .arg(root().join("scripts/render-deploy.sh"))
        .arg(domain)
        .arg(out)
        .current_dir(root())
        .output()
        .unwrap()
        .status
}

#[test]
fn the_render_script_refuses_an_embedded_domain_and_writes_nothing() {
    let declared = declared_embedded(&read("scripts/render-deploy.sh"), "render-deploy.sh");
    assert_eq!(
        declared,
        embedded_domains(),
        "render-deploy.sh EMBEDDED_DOMAINS"
    );
    for d in embedded_domains() {
        let out = tempfile::tempdir().unwrap();
        let target = out.path().join("rendered");
        let status = render(&d, &target);
        assert_eq!(status.code(), Some(2), "render {d} must refuse");
        assert!(!target.exists(), "a refused render must not write");
    }
}

/// Run a lifecycle playbook's shell with its template expressions filled from a
/// resource, and a fake `kubectl` that records every call.
fn run_lifecycle(verb: &str, path: &str, mode: Option<&str>) -> (Option<i32>, String, String) {
    let text = read(&format!(
        "playbooks/automation/agents/ehgraph/lifecycle/{verb}.yaml"
    ));
    let start = text.find("        - |\n").expect("one shell block") + "        - |\n".len();
    let script: String = text[start..]
        .lines()
        .map(|l| l.strip_prefix("          ").unwrap_or(l.trim_start()))
        .collect::<Vec<_>>()
        .join("\n");
    let domain = path.rsplit('/').next().unwrap();
    let mut filled = String::new();
    let mut rest = script.as_str();
    while let Some(open) = rest.find("{{") {
        filled.push_str(&rest[..open]);
        let close = rest[open..].find("}}").unwrap() + open;
        let expr = &rest[open + 2..close];
        let value = if expr.contains("deployment.mode") {
            mode.unwrap_or("standalone").to_string()
        } else if expr.contains("deployment.domain") {
            domain.to_string()
        } else if expr.contains("deployment.namespace") {
            "ehgraph".to_string()
        } else if expr.contains("metadata.name") {
            format!("ehgraph-{domain}")
        } else if expr.trim() == "workload.mcp_resource.path" {
            path.to_string()
        } else {
            panic!("{verb}: template expression this test does not fill: {expr}");
        };
        filled.push_str(&value);
        rest = &rest[close + 2..];
    }
    filled.push_str(rest);

    let bin = tempfile::tempdir().unwrap();
    let calls = bin.path().join("calls");
    let kubectl = bin.path().join("kubectl");
    fs::write(
        &kubectl,
        format!("#!/bin/sh\necho \"$@\" >> '{}'\n", calls.display()),
    )
    .unwrap();
    Command::new("chmod")
        .arg("+x")
        .arg(&kubectl)
        .status()
        .unwrap();
    let out = Command::new("bash")
        .arg("-c")
        .arg(&filled)
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .unwrap();
    let recorded = fs::read_to_string(&calls).unwrap_or_default();
    (
        out.status.code(),
        recorded,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn mutating_lifecycle_playbooks_refuse_embedded_domains_before_kubectl() {
    for verb in MUTATING_VERBS {
        let file = format!("lifecycle/{verb}.yaml");
        let text = read(&format!("playbooks/automation/agents/ehgraph/{file}"));
        assert_eq!(
            declared_embedded(&text, &file),
            embedded_domains(),
            "{file}"
        );

        for d in embedded_domains() {
            let path = format!("mcp/ehgraph/{d}");
            // Marked embedded, and a hand-written resource that forgot the mode.
            for mode in [Some("embedded"), None] {
                let (code, calls, stderr) = run_lifecycle(verb, &path, mode);
                assert_eq!(code, Some(2), "{verb} {d} mode={mode:?}: {stderr}");
                assert!(calls.is_empty(), "{verb} {d} reached kubectl: {calls}");
                assert!(stderr.contains("refusing"), "{stderr}");
            }
        }

        // A standalone domain still goes through to kubectl.
        let (code, calls, stderr) = run_lifecycle(verb, "mcp/ehgraph/network", None);
        assert_eq!(code, Some(0), "{verb} network: {stderr}");
        if verb != "deploy" {
            assert!(calls.contains("ehgraph-network"), "{verb}: {calls}");
        }
    }
}
