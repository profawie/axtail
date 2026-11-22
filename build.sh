#!/usr/bin/env sh
# Build the release binary and place it in the repository root as ./axtail
set -eu
cd "$(dirname "$0")"
cargo build --release
cp target/release/axtail ./axtail
echo "Built ./axtail"
