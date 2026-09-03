#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_dir"

target=${TOKEN_MONITOR_RUST_TARGET:-aarch64-apple-darwin}
cargo build --release --target "$target" --bin token-monitor
binary="$repo_dir/target/$target/release/token-monitor"
if [ ! -x "$binary" ]; then
  binary="$repo_dir/target/release/token-monitor"
fi

printf '%s\n' "$binary"
