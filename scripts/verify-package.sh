#!/usr/bin/env bash
set -euo pipefail

for crate in t3-rust-bridge t3-maki-tools t3-native-sqlite; do
  cargo clippy --locked --all-targets --manifest-path "crates/$crate/Cargo.toml" -- -D warnings
  cargo test --locked --manifest-path "crates/$crate/Cargo.toml"
done
node scripts/t3-accel.mjs build
node --test crates/t3-native-sqlite/test.mjs scripts/t3-accel.test.mjs
node scripts/t3-accel.mjs package "${1:-dist}"
