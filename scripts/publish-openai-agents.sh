#!/usr/bin/env bash
# Build, check and publish the OpenAI Agents SDK provider (chainguard-sandbox-openai-agents) to PyPI.
#
#   scripts/publish-openai-agents.sh               check only: test, type-check, build, then install
#                                                  the built wheel into an empty environment and import it
#   scripts/publish-openai-agents.sh --live        the same, plus the demo agent run against a real
#                                                  sandbox from the built wheel (needs a saved login)
#   scripts/publish-openai-agents.sh --test-pypi   the checks, then upload to test.pypi.org (asks first)
#   scripts/publish-openai-agents.sh --publish     the checks, then upload to pypi.org (asks first)
#
# It depends on chainguard-sandbox-sdk from PyPI, so publish that first (scripts/publish-python.sh).
# Until then the checks install the SDK built from python/ next to it.
# The version comes from integrations/openai-agents/pyproject.toml. Needs: uv. For uploads: an API
# token in UV_PUBLISH_TOKEN. See PUBLISHING.md.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
PKG=$ROOT/integrations/openai-agents
target="" live=false
while [[ $# -gt 0 ]]; do
  case $1 in
    --publish) target=pypi ;;
    --test-pypi) target=testpypi ;;
    --live) live=true ;;
    *) echo "usage: $0 [--live] [--test-pypi | --publish]" >&2; exit 2 ;;
  esac
  shift
done

cd "$PKG"
read -r name version < <(uv run --quiet python -c 'import tomllib;p=tomllib.load(open("pyproject.toml","rb"))["project"];print(p["name"],p["version"])')
echo "== $name $version"

index=https://pypi.org
[[ $target == testpypi ]] && index=https://test.pypi.org
if [[ $(curl -s -o /dev/null -w '%{http_code}' "$index/pypi/$name/$version/json") == 200 ]]; then
  echo "error: $name $version is already on $index; raise \"version\" in integrations/openai-agents/pyproject.toml" >&2
  exit 1
fi
if [[ -n $target && $(curl -s -o /dev/null -w '%{http_code}' "$index/pypi/chainguard-sandbox-sdk/json") != 200 ]]; then
  echo "error: chainguard-sandbox-sdk isn't on $index yet; publish it first (scripts/publish-python.sh)" >&2
  exit 1
fi

echo "== test and type-check"
uv sync --quiet
uv run --quiet pytest -q tests/test_offline.py
uv run --quiet pyright

echo "== build"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
uv build --quiet --out-dir "$work/dist"
(cd "$ROOT/python" && uv build --quiet --wheel --out-dir "$work/sdk")
ls "$work/dist" | sed 's/^/  /'
wheel=$(ls "$work"/dist/*.whl)
if uv run --quiet python -m zipfile -l "$wheel" | grep -qE '(^| )(tests|examples)/'; then
  echo "error: the wheel contains tests or examples" >&2; exit 1
fi
grep -q "^Requires-Dist: chainguard-sandbox-sdk" <(unzip -p "$wheel" '*.dist-info/METADATA') \
  || { echo "error: the wheel doesn't declare its chainguard-sandbox-sdk dependency" >&2; exit 1; }

echo "== install the wheel into an empty environment"
uv venv --quiet "$work/venv"
uv pip install --quiet --python "$work/venv/bin/python" "$work"/sdk/*.whl "$wheel"
(cd "$work" && "$work/venv/bin/python" -c '
import chainguard_sandbox_openai_agents as m
from agents.sandbox.session import BaseSandboxClientOptions
assert type(BaseSandboxClientOptions.parse({"type": "chainguard_workspaces"})).__name__ == "ChainguardSandboxClientOptions"
print("import ok")')
if $live; then
  (cd "$work" && "$work/venv/bin/python" "$PKG/examples/demo.py" | tail -1)
fi

if [[ -z $target ]]; then
  echo
  echo "All checks passed. Nothing was uploaded. To publish $name $version: $0 --test-pypi, then $0 --publish"
  exit 0
fi

[[ -n ${UV_PUBLISH_TOKEN:-} ]] || { echo "error: set UV_PUBLISH_TOKEN to an API token from $index/manage/account/token/" >&2; exit 1; }
echo
read -r -p "Upload $name $version to $index? A version can't be replaced once uploaded. Type the version to confirm: " answer
[[ $answer == "$version" ]] || { echo "not uploaded"; exit 1; }
if [[ $target == testpypi ]]; then
  uv publish --publish-url https://test.pypi.org/legacy/ "$work"/dist/*
else
  uv publish "$work"/dist/*
fi
echo "published: $index/project/$name/$version/"
