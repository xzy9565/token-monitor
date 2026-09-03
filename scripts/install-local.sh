#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$repo_dir/Cargo.toml" | head -1)
version=${version:-0.1.0}
user_home=${HOME:?HOME is required}
install_root=${TOKEN_MONITOR_INSTALL_ROOT:-$repo_dir/.releases}
release_dir="$install_root/releases/$version"

target=${TOKEN_MONITOR_RUST_TARGET:-aarch64-apple-darwin}
if [ -n "${TOKEN_MONITOR_BINARY:-}" ]; then
  binary=$TOKEN_MONITOR_BINARY
elif [ -x "$repo_dir/target/$target/release/token-monitor" ]; then
  # Prefer the artifact produced by build-release.sh. Falling back to the
  # host target keeps local development convenient, but must never silently
  # replace a cross-target release with an older host binary.
  binary="$repo_dir/target/$target/release/token-monitor"
else
  binary="$repo_dir/target/release/token-monitor"
fi
if [ ! -x "$binary" ]; then
  printf '%s\n' "release binary not found: $binary" >&2
  printf '%s\n' "run scripts/build-release.sh first" >&2
  exit 1
fi

mkdir -p "$install_root/releases"
staging=$(mktemp -d "$install_root/.install-$version.XXXXXX")
cleanup() { rm -rf "$staging"; }
trap cleanup EXIT INT TERM
cp "$binary" "$staging/token-monitor"
chmod 755 "$staging/token-monitor"
mkdir -p "$release_dir"
mv "$staging/token-monitor" "$release_dir/token-monitor"
ln -sfn "releases/$version" "$install_root/current"

printf '%s\n' "installed $release_dir/token-monitor"
printf '%s\n' "current release: $install_root/current"
