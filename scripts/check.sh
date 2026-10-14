#!/bin/sh
# What CI checks, run locally before a commit: formatting, clippy with
# warnings as errors, every test, and that docs/configuration.md matches
# the shipped configuration. Exits at the first failure.
set -eu
cd "$(dirname "$0")/.."
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -q -p hearth --bin hearth
./target/debug/hearth config --markdown | diff -u docs/configuration.md -
echo "all checks passed"
