#!/usr/bin/env node
// The installed `chainguard-sandbox` command. Bun runs the TypeScript CLI next to this file;
// Node.js runs the compiled one in dist/ (made by `npm run build`, included in the published package).
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";

const source = new URL("./chainguard-sandbox.ts", import.meta.url);
const built = new URL("../dist/bin/chainguard-sandbox.js", import.meta.url);
if (process.versions.bun && existsSync(fileURLToPath(source))) {
  await import(source.href);
} else if (existsSync(fileURLToPath(built))) {
  await import(built.href);
} else {
  console.error("chainguard-sandbox: this copy isn't built for Node.js: run `npm install && npm run build` in " + fileURLToPath(new URL("..", import.meta.url)) + ", or run it with Bun");
  process.exit(1);
}
