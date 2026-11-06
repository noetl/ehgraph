//! Test helper: hold the data-dir lock until stdin closes.
//! Not part of the supported server surface.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use ehgraph_store::{Store, StoreConfig};

fn main() {
    let dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: ehgraph-hold <data-dir>"),
    );
    let cfg = StoreConfig::for_tests(dir);
    match Store::open(cfg) {
        Ok(store) => {
            let mut out = io::stdout();
            writeln!(out, "READY").ok();
            out.flush().ok();
            let _ = io::stdin().lock().lines().next();
            drop(store);
        }
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    }
}
