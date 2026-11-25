#!/usr/bin/env bash
set -euo pipefail

cargo clippy --workspace --locked --all-targets -- -D warnings
cargo test --workspace --locked
node scripts/t3-accel.mjs build
node --test crates/t3-native-sqlite/test.mjs scripts/t3-accel.test.mjs
node scripts/t3-accel.mjs package "${1:-dist}"
