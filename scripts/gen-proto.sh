#!/usr/bin/env bash
# Generate the Go, Python, TypeScript and Rust code for proto/sandbox/runtime/session/v1/session.proto.
#
#   scripts/gen-proto.sh            all four
#   scripts/gen-proto.sh go|python|typescript|rust
#
# Needs protoc (28.x) and:
#   go          protoc-gen-go v1.36.6 and protoc-gen-go-grpc v1.5.1 on PATH (or GOBIN=...):
#                 go install google.golang.org/protobuf/cmd/protoc-gen-go@v1.36.6
#                 go install google.golang.org/grpc/cmd/protoc-gen-go-grpc@v1.5.1
#   python      uv (grpcio-tools is fetched on demand)
#   typescript  bun, with `bun install` run in typescript/ (protoc-gen-es is a dev dependency)
#   rust        cargo (rust/gen runs tonic-prost-build, which calls protoc)
set -euo pipefail
cd "$(dirname "$0")/.."
PROTO=sandbox/runtime/session/v1/session.proto
want=${1:-all}

if [[ $want == all || $want == go ]]; then
  [[ -n ${GOBIN:-} ]] && export PATH="$GOBIN:$PATH"
  out=go/api/sessionv1
  rm -rf "$out" && mkdir -p "$out"
  protoc -I proto \
    --go_out=go --go_opt=module=github.com/sandboxing-dev/chainguard-sandbox-sdk/go \
    --go-grpc_out=go --go-grpc_opt=module=github.com/sandboxing-dev/chainguard-sandbox-sdk/go \
    "$PROTO"
  echo "go: $out"
fi

if [[ $want == all || $want == python ]]; then
  out=python/src/chainguard_sandbox/workspaces/_gen
  rm -rf "$out" && mkdir -p "$out"
  uvx --quiet --from 'grpcio-tools>=1.85.1' python -m grpc_tools.protoc -I proto \
    --python_out="$out" --pyi_out="$out" --grpc_python_out="$out" "$PROTO"
  # the stubs import "sandbox.runtime..."; make that "chainguard_sandbox.workspaces._gen.sandbox.runtime..."
  find "$out" \( -name '*.py' -o -name '*.pyi' \) -print0 | while IFS= read -r -d '' f; do
    sed -i.bak -E 's/^from sandbox\.runtime\./from chainguard_sandbox.workspaces._gen.sandbox.runtime./' "$f" && rm -f "$f.bak"
  done
  find "$out" -type d -exec touch {}/__init__.py \;
  echo "python: $out"
fi

if [[ $want == all || $want == typescript ]]; then
  out=typescript/src/gen
  rm -rf "$out" && mkdir -p "$out"
  protoc -I proto \
    --plugin=protoc-gen-es=typescript/node_modules/.bin/protoc-gen-es \
    --es_out="$out" --es_opt=target=ts,import_extension=none \
    "$PROTO"
  echo "typescript: $out"
fi

if [[ $want == all || $want == rust ]]; then
  out=rust/src/workspaces/gen
  rm -rf "$out" && mkdir -p "$out"
  cargo run --quiet --manifest-path rust/gen/Cargo.toml -- proto "$out"
  echo "rust: $out"
fi
