#!/bin/sh
# What CI checks, run locally before a commit: formatting, clippy with
# warnings as errors, every test, that docs/configuration.md matches the
# shipped configuration, and that no source file passes 800 lines. Exits
# at the first failure.
set -eu
cd "$(dirname "$0")/.."
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -q -p hearth --bin hearth
./target/debug/hearth config --markdown | diff -u docs/configuration.md -
# Files stay small enough to read whole: 800 lines at most.
long=$(find crates -name '*.rs' -exec wc -l {} + | awk '$2 != "total" && $1 > 800 { print $2 " (" $1 " lines)" }')
if [ -n "$long" ]; then
  echo "over 800 lines; split by concern:" >&2
  echo "$long" >&2
  exit 1
fi
echo "all checks passed"
