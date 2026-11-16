#!/usr/bin/env bash
# Build, check and publish the Rust crate (chainguard-sandbox-sdk) to crates.io.
#
#   scripts/publish-rust.sh              check only: test, lint, package, then build the packaged
#                                        crate in an empty project and run its CLI
#   scripts/publish-rust.sh --live       the same, plus a real sandbox created from the packaged
#                                        crate (needs a saved login; deletes the sandbox)
#   scripts/publish-rust.sh --publish    the checks, then upload to crates.io (asks first)
#
# The version comes from rust/Cargo.toml; crates.io refuses a version that was published before.
# Needs: cargo. For --publish: `cargo login` done on this machine (an API token from
# crates.io/settings/tokens), and a committed working tree. See PUBLISHING.md.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
PKG=$ROOT/rust
publish=false live=false
while [[ $# -gt 0 ]]; do
  case $1 in
    --publish) publish=true ;;
    --live) live=true ;;
    *) echo "usage: $0 [--live] [--publish]" >&2; exit 2 ;;
  esac
  shift
done

cd "$PKG"
meta=$(cargo metadata --no-deps --format-version 1)
name=$(printf '%s' "$meta" | python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["name"])')
version=$(printf '%s' "$meta" | python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])')
echo "== $name $version"

if [[ $(curl -s -o /dev/null -w '%{http_code}' -A 'chainguard-sandbox-sdk-publish-script' "https://crates.io/api/v1/crates/$name/$version") == 200 ]]; then
  echo "error: $name $version is already on crates.io; raise \"version\" in rust/Cargo.toml" >&2
  exit 1
fi

echo "== test, lint, docs"
cargo test --quiet
cargo clippy --quiet --all-targets -- -D warnings
cargo fmt --check
RUSTDOCFLAGS="-D warnings" cargo doc --quiet --no-deps

echo "== package"
cp "$ROOT/LICENSE" "$ROOT/NOTICE" "$PKG/"
work=$(mktemp -d)
trap 'rm -rf "$work" "$PKG/LICENSE" "$PKG/NOTICE"' EXIT
dirty=()
$publish || dirty=(--allow-dirty)
cargo package --quiet ${dirty[@]+"${dirty[@]}"}
crate=$PKG/target/package/$name-$version.crate
cargo package --list ${dirty[@]+"${dirty[@]}"} | sed 's/^/  /' | grep -v '^  src/workspaces/gen/' | sort
echo "  ($(cargo package --list ${dirty[@]+"${dirty[@]}"} | wc -l | tr -d ' ') files, $(du -h "$crate" | cut -f1 | tr -d ' '))"
if cargo package --list ${dirty[@]+"${dirty[@]}"} | grep -qE '^(target|gen)/'; then
  echo "error: the package contains build output or the generator" >&2; exit 1
fi

echo "== build the packaged crate in an empty project"
mkdir -p "$work/unpacked" && tar -xzf "$crate" -C "$work/unpacked"
src=$work/unpacked/$name-$version
cargo new --quiet "$work/app"
cd "$work/app"
cargo add --quiet "$name" --path "$src"
cargo add --quiet tokio --features macros,rt-multi-thread
cp "$ROOT/scripts/smoke/rust_smoke.rs" src/main.rs
smoke_args=()
$live && smoke_args+=(--live)
cargo run --quiet -- ${smoke_args[@]+"${smoke_args[@]}"}

echo "== install the CLI from the packaged crate"
cargo install --quiet --path "$src" --root "$work/bin"
usage=$("$work/bin/bin/chainguard-sandbox" 2>&1 || true)
[[ $usage == usage:\ chainguard-sandbox* ]] || { echo "error: the chainguard-sandbox command did not run: $usage" >&2; exit 1; }
echo "command ok"

if ! $publish; then
  echo
  echo "All checks passed. Nothing was uploaded. To publish $name $version: $0 --publish"
  exit 0
fi

cd "$PKG"
echo
read -r -p "Publish $name $version to crates.io? This can't be undone. Type the version to confirm: " answer
[[ $answer == "$version" ]] || { echo "not published"; exit 1; }
cargo publish
echo "published: https://crates.io/crates/$name/$version"
