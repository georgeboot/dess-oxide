#!/usr/bin/env bash
# Exactly what CI and the release workflow check. Run before every push or tag.
set -euo pipefail
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
