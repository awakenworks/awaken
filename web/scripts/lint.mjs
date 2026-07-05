#!/usr/bin/env node
// Console lint: (1) single-egress fetch — only lib/api/client.ts may call
// fetch/EventSource-with-headers; (2) surface files stay under the size cap.

import { readFileSync, readdirSync, statSync } from "node:fs";
import { join, relative } from "node:path";

const ROOT = new URL("../src", import.meta.url).pathname;
const FETCH_ALLOWED = new Set(["lib/api/client.ts"]);
const MAX_LINES = 600;

let failed = false;

function walk(dir) {
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) {
      walk(path);
      continue;
    }
    if (!/\.(ts|tsx)$/.test(name)) continue;
    const rel = relative(ROOT, path);
    const text = readFileSync(path, "utf8");
    if (!FETCH_ALLOWED.has(rel) && /\bfetch\s*\(/.test(text)) {
      console.error(`no-raw-fetch: ${rel} calls fetch() — go through lib/api/client.ts`);
      failed = true;
    }
    const lines = text.split("\n").length;
    if (lines > MAX_LINES) {
      console.error(`file-size: ${rel} is ${lines} lines (> ${MAX_LINES}) — split the surface`);
      failed = true;
    }
  }
}

walk(ROOT);
if (failed) process.exit(1);
console.log("OK - console lint passed.");
