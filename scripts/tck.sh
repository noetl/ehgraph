#!/usr/bin/env bash
# Run the openCypher TCK against ehgraph's Cypher engine.
#
#   scripts/tck.sh [workdir]
#
# Clones openCypher at the pinned commit into <workdir> (default: a temp dir; the
# feature files are Apache-2.0 and are not vendored into this repo), then runs the
# ignored `opencypher_tck` test in crates/ehgraph/tests/tck.rs. The scenario-level
# report is written to $EHGRAPH_TCK_OUT (default target/tck-results.json).
#
# EHGRAPH_TCK_FILTER=clauses/match  runs only feature files whose path contains it.
set -euo pipefail

TCK_REPO="https://github.com/opencypher/openCypher"
TCK_TAG="2024.3"
TCK_COMMIT="677cbafabb8c3c5eed458fd3b1ec0daec8d67d23"

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
workdir="${1:-$(mktemp -d)}"
checkout="$workdir/openCypher"

if [ ! -d "$checkout/.git" ]; then
  git clone --quiet "$TCK_REPO" "$checkout"
fi
git -C "$checkout" fetch --quiet --tags origin
git -C "$checkout" -c advice.detachedHead=false checkout --quiet "$TCK_COMMIT"
echo "openCypher TCK $TCK_TAG @ $(git -C "$checkout" rev-parse HEAD)"

export EHGRAPH_TCK_DIR="$checkout/tck"
export EHGRAPH_TCK_OUT="${EHGRAPH_TCK_OUT:-$repo_root/target/tck-results.json}"
mkdir -p "$(dirname "$EHGRAPH_TCK_OUT")"
cd "$repo_root"
cargo test --release -p ehgraph --test tck -- --ignored --nocapture opencypher_tck
