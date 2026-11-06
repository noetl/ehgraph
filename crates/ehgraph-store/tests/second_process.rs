use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use ehgraph_store::StoreConfig;

#[test]
fn second_process_on_the_same_data_dir_refuses_to_start() {
    // The helper binary is built beside this test.
    let dir = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_ehgraph-hold");
    let mut child = Command::new(bin)
        .arg(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "READY", "holder did not start: {line}");

    let second = Command::new(bin).arg(dir.path()).output().unwrap();
    assert!(
        !second.status.success(),
        "second process started: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("locked"),
        "expected the data-dir lock error, got {stderr}"
    );

    child.stdin.take().unwrap().write_all(b"\n").unwrap();
    assert!(child.wait().unwrap().success());

    // The lock is released, so a later open works.
    std::thread::sleep(Duration::from_millis(20));
    let cfg = StoreConfig::for_tests(dir.path().to_path_buf());
    ehgraph_store::Store::open(cfg).unwrap();
}
