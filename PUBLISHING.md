# Publishing

How to publish the Python package to PyPI and the TypeScript package to npm, from your own machine. The Go
module needs no registry: it is published by pushing a tag (see [Go](#go)).

Both packages are named `chainguard-sandbox-sdk`. Both install a command named `chainguard-sandbox`.

## What You Need

| | PyPI (Python) | npm (TypeScript) |
|---|---|---|
| Account | [pypi.org/account/register](https://pypi.org/account/register/) | [npmjs.com/signup](https://www.npmjs.com/signup) |
| Two-factor authentication | required before you can upload | required for publishing |
| Credential on this machine | an API token in `UV_PUBLISH_TOKEN` | `npm login` (opens the browser) |
| Tools | `uv` | `bun`, `node`, `npm` |

A separate account on [test.pypi.org](https://test.pypi.org/account/register/) (with its own token) lets you
rehearse the Python upload. npm has no test registry; `scripts/publish-typescript.sh` without `--publish` is
the rehearsal.

**PyPI token.** Create it at [pypi.org/manage/account/token](https://pypi.org/manage/account/token/). The
first upload creates the project, so the first token has to be scoped to your whole account. After the
first upload, delete it and create one scoped to the `chainguard-sandbox-sdk` project only. Keep the token
out of files in this repository: export it in the terminal you publish from.

```sh
export UV_PUBLISH_TOKEN=pypi-...
```

**npm login.** `npm login` stores a session in `~/.npmrc`; `npm whoami` shows who you are logged in as.
Each publish asks for the second factor.

**Who owns the package.** The first upload makes your personal account the owner on each registry. To own
the packages as a group instead: on PyPI, create an organization and transfer the project to it; on npm,
create an organization and publish under its scope (`@your-org/chainguard-sandbox-sdk`), which means
changing `name` in `typescript/package.json` and the import paths in the docs. Adding more owners to an
unscoped package works on both registries without any of that.

## Before the First Publish

- **The names are permanent in practice.** Check them once more:
  `npm view chainguard-sandbox-sdk` and `https://pypi.org/project/chainguard-sandbox-sdk/` should both
  say "not found".
- **The name includes "chainguard" and the SDK is unofficial.** Both READMEs say so; registries act on
  trademark complaints by transferring or removing packages.
- **A version can't be replaced.** Neither registry accepts a second upload of the same version, even
  after deleting it. A mistake is fixed by publishing the next version.

## Releasing a Version

1. Set the same version in `python/pyproject.toml` and `typescript/package.json`
   ([semantic versioning](https://semver.org/); stay below 1.0.0 while the API can still change).
2. Run the checks. Nothing is uploaded:

    ```sh
    scripts/publish-python.sh --live
    scripts/publish-typescript.sh --live
    ```

    Each script runs the tests, builds the package, installs the built file into an empty environment and
    loads it there. `--live` also creates a real sandbox from the built package and deletes it (it needs a
    saved login and an organization with Workspaces on).
3. Rehearse the Python upload on TestPyPI, with a test.pypi.org token in `UV_PUBLISH_TOKEN`:

    ```sh
    scripts/publish-python.sh --test-pypi
    ```

4. Publish. Each script asks you to type the version before it uploads:

    ```sh
    scripts/publish-python.sh --publish        # pypi.org token in UV_PUBLISH_TOKEN
    scripts/publish-typescript.sh --publish    # after npm login
    ```

5. Check the result from an empty directory:

    ```sh
    uvx --from chainguard-sandbox-sdk chainguard-sandbox whoami
    npx --package chainguard-sandbox-sdk chainguard-sandbox whoami
    ```

6. Tag the commit you published from, and push the tags (this is also the Go release):

    ```sh
    git tag v0.1.0 && git tag go/v0.1.0
    git push origin v0.1.0 go/v0.1.0
    ```

7. Update the install instructions in the READMEs and the docs to the registry form (`uv add
   chainguard-sandbox-sdk`, `bun add chainguard-sandbox-sdk`, `npm install chainguard-sandbox-sdk`).

A pre-release goes out the same way with a pre-release version (`0.2.0rc1` for Python, `0.2.0-rc.1` for
TypeScript) and, on npm, `--tag next` so that a plain `npm install` keeps getting the last full release.

## If Something Goes Wrong

- **A broken version is out.** Publish a fixed one. Then mark the broken one: on PyPI, "yank" it from the
  project's release page (installers skip it unless it is asked for by exact version); on npm,
  `npm deprecate chainguard-sandbox-sdk@0.1.0 "use 0.1.1"`.
- **npm unpublish** works only within 72 hours and only if nothing depends on the version; the version
  number stays used either way.
- **A token leaked.** Delete it on the registry's token page and create a new one.

## What the Packages Contain

- **Python:** a source archive and a wheel built by `uv build`: the `chainguard_sandbox` package, no tests
  or examples.
- **TypeScript:** `dist/` (JavaScript, type declarations and source maps, built by `tsc -p
  tsconfig.build.json`), `src/` (the TypeScript, which Bun runs directly), `bin/`, the README, and copies of
  `LICENSE` and `NOTICE`. Node.js uses `dist/`; Bun uses `src/`.

## Go

Go modules are fetched from the repository, so there is no upload. The module lives in the `go/`
subdirectory, so its version tags carry that prefix: `go/v0.1.0`. After the tag is pushed,
`go get github.com/sandboxing-dev/chainguard-sandbox-sdk/go@v0.1.0` works, and the package page appears on
[pkg.go.dev](https://pkg.go.dev/) the first time someone requests it. A pushed Go version is cached by the
public module proxy for good: fix mistakes with a new tag.
