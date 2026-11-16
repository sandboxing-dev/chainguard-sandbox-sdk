#!/usr/bin/env bash
# Build, check and publish the Python package (chainguard-sandbox-sdk) to PyPI.
#
#   scripts/publish-python.sh               check only: test, build, then install the built wheel
#                                           into an empty environment and import it
#   scripts/publish-python.sh --live        the same, plus a real sandbox created from the built
#                                           wheel (needs a saved login; deletes the sandbox)
#   scripts/publish-python.sh --test-pypi   the checks, then upload to test.pypi.org (asks first)
#   scripts/publish-python.sh --publish     the checks, then upload to pypi.org (asks first)
#
# The version comes from python/pyproject.toml; PyPI refuses a version that was uploaded before.
# Needs: uv. For uploads: an API token in UV_PUBLISH_TOKEN (a pypi.org token for --publish, a
# test.pypi.org token for --test-pypi). See PUBLISHING.md.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
PKG=$ROOT/python
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
name=$(uv run --quiet python -c 'import tomllib;print(tomllib.load(open("pyproject.toml","rb"))["project"]["name"])')
version=$(uv run --quiet python -c 'import tomllib;print(tomllib.load(open("pyproject.toml","rb"))["project"]["version"])')
echo "== $name $version"

index=https://pypi.org
[[ $target == testpypi ]] && index=https://test.pypi.org
if [[ $(curl -s -o /dev/null -w '%{http_code}' "$index/pypi/$name/$version/json") == 200 ]]; then
  echo "error: $name $version is already on $index; raise \"version\" in python/pyproject.toml" >&2
  exit 1
fi

echo "== test and typecheck"
uv sync --quiet
uv run --quiet pytest -q
uv run --quiet pyright

echo "== build"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
uv build --quiet --out-dir "$work/dist"
ls "$work/dist" | sed 's/^/  /'
wheel=$(ls "$work"/dist/*.whl)
echo "  wheel contents:"
uv run --quiet python -m zipfile -l "$wheel" | awk 'NR>1 {print "    " $1}' | grep -v '/_gen/.*_pb2' | sort
if uv run --quiet python -m zipfile -l "$wheel" | grep -qE '(^| )(tests|examples)/'; then
  echo "error: the wheel contains tests or examples" >&2; exit 1
fi

echo "== install the wheel into an empty environment"
uv venv --quiet "$work/venv"
uv pip install --quiet --python "$work/venv/bin/python" "$wheel"
smoke_args=()
$live && smoke_args+=(--live)
(cd "$work" && "$work/venv/bin/python" "$ROOT/scripts/smoke/python_smoke.py" ${smoke_args[@]+"${smoke_args[@]}"})
usage=$("$work/venv/bin/chainguard-sandbox" 2>&1 || true)
[[ $usage == *usage* ]] || { echo "error: the chainguard-sandbox command did not run: $usage" >&2; exit 1; }
echo "command ok"

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
