#!/usr/bin/env bash
# Build, check and publish the TypeScript package (chainguard-sandbox-sdk) to npm.
#
#   scripts/publish-typescript.sh              check only: test, build, pack, then install the packed
#                                              file into empty projects and load it in Node.js and Bun
#   scripts/publish-typescript.sh --live       the same, plus a real sandbox created from the packed
#                                              package (needs a saved login; deletes the sandbox)
#   scripts/publish-typescript.sh --publish    the checks, then upload to npm (asks first)
#
# Options combine. --tag NAME publishes under an npm tag other than "latest" (e.g. "next").
# The version comes from typescript/package.json; npm refuses a version that was published before.
# Needs: bun, node, npm. For --publish: `npm login` done on this machine. See PUBLISHING.md.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
PKG=$ROOT/typescript
publish=false live=false tag=latest
while [[ $# -gt 0 ]]; do
  case $1 in
    --publish) publish=true ;;
    --live) live=true ;;
    --tag) tag=${2:?--tag needs a name}; shift ;;
    *) echo "usage: $0 [--live] [--publish] [--tag NAME]" >&2; exit 2 ;;
  esac
  shift
done

# Run a command with a time limit, up to three times. Bun 1.3 occasionally stalls while starting a
# program that loads the gRPC transport with its output on a pipe; a stalled start is retried.
bun_try() {
  local attempt pid waited
  for attempt in 1 2 3; do
    "$@" & pid=$!
    waited=0
    while kill -0 "$pid" 2>/dev/null && (( waited < 300 )); do sleep 0.1; waited=$((waited + 1)); done
    if kill -0 "$pid" 2>/dev/null; then
      kill -9 "$pid" 2>/dev/null; wait "$pid" 2>/dev/null || true
      echo "  (bun stalled while starting; attempt $attempt of 3)" >&2
      continue
    fi
    wait "$pid"; return $?
  done
  echo "error: bun stalled three times running: $*" >&2; return 1
}

cd "$PKG"
name=$(node -p 'require("./package.json").name')
version=$(node -p 'require("./package.json").version')
echo "== $name $version"

if published=$(npm view "$name@$version" version 2>/dev/null) && [[ -n $published ]]; then
  echo "error: $name $version is already on npm; raise \"version\" in typescript/package.json" >&2
  exit 1
fi

echo "== install, typecheck, test"
bun install --frozen-lockfile
bun run typecheck
bun test

echo "== build"
bun run build
cp "$ROOT/LICENSE" "$ROOT/NOTICE" "$PKG/"
work=$(mktemp -d)
trap 'rm -rf "$work" "$PKG/LICENSE" "$PKG/NOTICE"' EXIT

echo "== pack"
tarball=$work/$(npm pack --silent --cache "$work/npm-cache" --pack-destination "$work")
tar -tzf "$tarball" | sed 's/^package\//  /' | grep -v '\.map$' | sort
echo "  ($(tar -tzf "$tarball" | wc -l | tr -d ' ') files, $(du -h "$tarball" | cut -f1 | tr -d ' '))"

smoke_args=()
$live && smoke_args+=(--live)

echo "== install the packed file into an empty project: Node.js $(node --version)"
mkdir "$work/node-app" && cd "$work/node-app"
npm init -y >/dev/null
npm pkg set type=module >/dev/null
# its own cache: the check should not depend on, or change, the state of ~/.npm
npm install --loglevel=error --no-audit --no-fund --cache "$work/npm-cache" "$tarball"
cp "$ROOT/scripts/smoke/node-smoke.mjs" .
node node-smoke.mjs ${smoke_args[@]+"${smoke_args[@]}"}
# with no arguments the command prints its usage and exits 2
usage=$(npx --no-install chainguard-sandbox 2>&1 || true)
[[ $usage == usage:\ chainguard-sandbox* ]] || { echo "error: the chainguard-sandbox command did not run under Node.js: $usage" >&2; exit 1; }
echo "command ok (node)"

echo "== install the packed file into an empty project: Bun $(bun --version)"
mkdir "$work/bun-app" && cd "$work/bun-app"
bun init -y >/dev/null 2>&1
bun add "$tarball" >/dev/null 2>&1
cp "$ROOT/scripts/smoke/node-smoke.mjs" .
bun_try bun node-smoke.mjs
usage=$(bun_try bun run chainguard-sandbox 2>&1 || true)
[[ $usage == *usage:\ chainguard-sandbox* ]] || { echo "error: the chainguard-sandbox command did not run under Bun: $usage" >&2; exit 1; }
echo "command ok (bun)"

if ! $publish; then
  echo
  echo "All checks passed. Nothing was uploaded. To publish $name $version: $0 --publish"
  exit 0
fi

cd "$PKG"
who=$(npm whoami 2>/dev/null) || { echo "error: not logged in to npm; run: npm login" >&2; exit 1; }
echo
read -r -p "Publish $name $version to npm as \"$who\" with tag \"$tag\"? This can't be undone. Type the version to confirm: " answer
[[ $answer == "$version" ]] || { echo "not published"; exit 1; }
npm publish "$tarball" --access public --tag "$tag"
echo "published: https://www.npmjs.com/package/$name/v/$version"
