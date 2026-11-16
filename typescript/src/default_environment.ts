// The document chainctl 0.2.398 synthesizes when a directory has no .chainguard/ files.
// A string in a module, so it loads the same way in Bun and in Node.js.
export default "# Copyright 2026 Chainguard, Inc.\n" +
  "# SPDX-License-Identifier: Apache-2.0\n" +
  "\n" +
  "# The default `chainctl develop` environment: one environment-only check with\n" +
  "# a shell and the everyday tools, nothing specific to any repository. A\n" +
  "# session compiled from it boots this shell; it runs no steps. Replace the\n" +
  "# packages with the toolchains the repository needs.\n" +
  "#\n" +
  "# Commit this file as .chainguard/ci.yaml to make it the repository's own \u2014\n" +
  "# but a check with nothing to run produces no verdict, so the check paths\n" +
  "# refuse it. Give `develop:` a `cmd:` before committing it, or keep it here\n" +
  "# as the session default it is.\n" +
  "version: 1\n" +
  "environment:\n" +
  "  packages:\n" +
  "    # The shell and its core.\n" +
  "    - bash\n" +
  "    - coreutils\n" +
  "    - make\n" +
  "    - curl\n" +
  "    - git\n" +
  "    # apk, so the session can install what this list lacks\n" +
  "    # (`apk add python-3.13`) without leaving the shell.\n" +
  "    - apk-tools\n" +
  "    # Coding agents. `claude` is the Claude Code CLI; `--with claude` lends it\n" +
  "    # this machine's login. (`codex` is packaged but published only to the\n" +
  "    # build-time-only private repo, which a session's own token does not\n" +
  "    # reach today; add it here once it is in a runtime-visible repo.)\n" +
  "    - claude\n" +
  "    # Everyday tools: GitHub, JSON, search, paging, editing. `gh` has no\n" +
  "    # credential: a session forwards git fetch and push for its own\n" +
  "    # repository and the checkout's other remotes, not the GitHub API.\n" +
  "    - gh\n" +
  "    - jq\n" +
  "    - ripgrep\n" +
  "    - less\n" +
  "    - vim\n" +
  "    # Cloud: the Google Cloud CLI. Sessions get Application Default\n" +
  "    # Credentials for the owning org's GCP account association, so gcloud\n" +
  "    # works as that account without a login.\n" +
  "    - google-cloud-sdk\n" +
  "    # Chainguard: chainctl. It acts as the session's owner without a login\n" +
  "    # through the `chainguard` credential, on by default at its narrow scope\n" +
  "    # (read and MCP capabilities at the session's organization); after\n" +
  "    # `--with none`, turn it on with `chainctl develop SESSION --with chainguard`.\n" +
  "    - chainctl\n" +
  "checks:\n" +
  "  develop:\n" +
  "    # Open egress: `egress` is the allowlist, and \"*\" allows every host. The\n" +
  "    # platform's mandatory deny floor (cloud metadata, loopback, private\n" +
  "    # ranges) still applies. `policy` is not a default; it is what happens to\n" +
  "    # a connection outside the allowlist or into the floor: `deny` refuses\n" +
  "    # the connection and the guest keeps running, `destroy` tears the guest\n" +
  "    # down.\n" +
  "    network:\n" +
  "      egress:\n" +
  "        - \"*\"\n" +
  "      policy: deny\n";
