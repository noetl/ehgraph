//! The on-disk format marker (E9): an older ehgraph refuses a data directory a newer
//! one wrote instead of silently ignoring what it does not understand.
//!
//! The rollback this guards: noetl-server embeds ehgraph, so rolling the host back
//! one release rolls ehgraph back onto a directory the newer build wrote. Each test
//! simulates "written by a newer build" by editing a file this build wrote, then
//! checks that open refuses, names the file, and leaves the directory as it was.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ehgraph_core::{ErrorCode, NodeInput, WriteRequest};
use ehgraph_store::format::{current_format, STORE_FORMAT};
use ehgraph_store::{Store, StoreConfig};
use serde_json::Value;

fn cfg(dir: &Path) -> StoreConfig {
    let mut cfg = StoreConfig::for_tests(dir.to_path_buf());
    // Seal after every commit, so the manifest and head pointer exist on disk.
    cfg.seal_max_age = Duration::ZERO;
    cfg
}

fn write_one(dir: &Path) {
    let store = Store::open(cfg(dir)).unwrap();
    store
        .upsert(WriteRequest {
            namespace: "lab".into(),
            nodes: vec![NodeInput {
                id: "device:sw1".into(),
                type_name: "Device".into(),
                props: Default::default(),
                origin: None,
                first_seen: Some(10),
                last_seen: Some(10),
            }],
            edges: vec![],
            idempotency_key: None,
            source: "format-test".into(),
            execution_id: "exec".into(),
            ts_ms: 10,
        })
        .unwrap();
}

fn manifest(dir: &Path) -> PathBuf {
    dir.join("namespaces/lab/manifest.json")
}

fn head(dir: &Path) -> PathBuf {
    dir.join("namespaces/lab/head.json")
}

fn instance(dir: &Path) -> PathBuf {
    dir.join("instance.json")
}

fn edit(path: &Path, f: impl FnOnce(&mut serde_json::Map<String, Value>)) {
    let mut value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    f(value.as_object_mut().unwrap());
    fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

/// Every file under `dir`, with its bytes, so a test can prove a refused open
/// wrote nothing.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in fs::read_dir(&next).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().unwrap() != "LOCK" {
                out.push((path.clone(), fs::read(&path).unwrap()));
            }
        }
    }
    out.sort();
    out
}

fn refused(dir: &Path) -> ehgraph_core::EhError {
    let before = snapshot(dir);
    let err = match Store::open(cfg(dir)) {
        Err(err) => err,
        Ok(_) => panic!("opened a data directory this build does not fully understand"),
    };
    assert_eq!(snapshot(dir), before, "a refused open must not write");
    err
}

#[test]
fn every_file_this_build_writes_carries_a_format_marker() {
    let dir = tempfile::tempdir().unwrap();
    write_one(dir.path());
    for path in [manifest(dir.path()), head(dir.path()), instance(dir.path())] {
        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            value["format"],
            Value::from(current_format()),
            "{} has no current format marker",
            path.display()
        );
    }
}

#[test]
fn a_manifest_from_a_newer_format_refuses_and_names_it() {
    let dir = tempfile::tempdir().unwrap();
    write_one(dir.path());
    edit(&manifest(dir.path()), |m| {
        m.insert("format".into(), Value::from(STORE_FORMAT + 1));
    });
    let err = refused(dir.path());
    assert_eq!(err.code, ErrorCode::Unavailable);
    assert!(err.message.contains("manifest.json"), "{}", err.message);
    assert!(
        err.message.contains("newer than this build"),
        "{}",
        err.message
    );
}

#[test]
fn an_unknown_manifest_field_refuses_even_without_a_format_bump() {
    let dir = tempfile::tempdir().unwrap();
    write_one(dir.path());
    // A newer build that added a field and forgot to bump the format.
    edit(&manifest(dir.path()), |m| {
        m.insert("retention_floor".into(), Value::from(7));
    });
    let err = refused(dir.path());
    assert!(err.message.contains("manifest.json"), "{}", err.message);
    assert!(err.message.contains("retention_floor"), "{}", err.message);
}

#[test]
fn an_unknown_field_in_a_nested_part_entry_refuses() {
    let dir = tempfile::tempdir().unwrap();
    write_one(dir.path());
    edit(&manifest(dir.path()), |m| {
        let parts = m.get_mut("parts").unwrap().as_array_mut().unwrap();
        assert!(!parts.is_empty(), "the test needs a sealed part");
        parts[0]
            .as_object_mut()
            .unwrap()
            .insert("codec".into(), Value::from("zstd"));
    });
    let err = refused(dir.path());
    assert!(err.message.contains("codec"), "{}", err.message);
}

#[test]
fn a_head_pointer_from_a_newer_format_refuses() {
    let dir = tempfile::tempdir().unwrap();
    write_one(dir.path());
    edit(&head(dir.path()), |m| {
        m.insert("format".into(), Value::from(STORE_FORMAT + 1));
    });
    let err = refused(dir.path());
    assert!(err.message.contains("head.json"), "{}", err.message);
}

#[test]
fn an_instance_file_from_a_newer_format_refuses() {
    let dir = tempfile::tempdir().unwrap();
    write_one(dir.path());
    edit(&instance(dir.path()), |m| {
        m.insert("format".into(), Value::from(STORE_FORMAT + 1));
    });
    let err = refused(dir.path());
    assert!(err.message.contains("instance.json"), "{}", err.message);
}

#[test]
fn files_written_before_the_marker_still_open() {
    // The MVP wrote no `format` field. Those files are format 1 and must open.
    let dir = tempfile::tempdir().unwrap();
    write_one(dir.path());
    for path in [manifest(dir.path()), head(dir.path()), instance(dir.path())] {
        edit(&path, |m| {
            m.remove("format");
        });
    }
    let store = Store::open(cfg(dir.path())).unwrap();
    assert_eq!(store.head("lab").unwrap(), 1);
    assert_eq!(store.graph_at("lab", None).unwrap().nodes.len(), 1);
}
