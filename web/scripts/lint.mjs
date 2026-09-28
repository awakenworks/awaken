#!/usr/bin/env node
// Console lint: (1) single-egress fetch — only lib/api/client.ts may call
// fetch/EventSource-with-headers; (2) surface files stay under the size cap.

import assert from "node:assert/strict";
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join, relative, sep } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = fileURLToPath(new URL("../src", import.meta.url));
const FETCH_ALLOWED = new Set(["lib/api/client.ts"]);
const SESSION_CREATE_ALLOWED = new Set(["lib/api/client.ts"]);
// Secret entry must go through the write-only SecretField seam (ADR-0038 invariant:
// a stored secret is never read back into the UI). @awaken/ui owns the only
// raw password control; the product adapter supplies copy only.
const MAX_LINES = 600;
// The opening `// @formal ...` lines of a source are the formal ledger's
// file-head anchors (scripts/ci/formal_anchors.py): metadata that moved out of
// formal/*.json into the file it classifies, not surface code. They do not
// count toward the size cap; an anchor line anywhere else does.
function surfaceLines(text) {
  const lines = text.split("\n");
  let head = 0;
  while (head < lines.length && lines[head].startsWith("// @formal ")) head += 1;
  return lines.length - head;
}
for (const [fixture, expected] of [
  ["a\nb", 2],
  ["// @formal surface requirement=x.y\n// @formal reason: r\na\nb", 2],
  ["a\n// @formal reason: r\nb", 3],
]) assert.equal(surfaceLines(fixture), expected);

let failed = false;

function walk(dir) {
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) {
      walk(path);
      continue;
    }
    if (!/\.(ts|tsx)$/.test(name)) continue;
    const rel = relative(ROOT, path).split(sep).join("/");
    const text = readFileSync(path, "utf8");
    if (!FETCH_ALLOWED.has(rel) && /\bfetch\s*\(/.test(text)) {
      console.error(`no-raw-fetch: ${rel} calls fetch() — go through lib/api/client.ts`);
      failed = true;
    }
    if (
      !SESSION_CREATE_ALLOWED.has(rel)
      && /\bapi\.post(?:<[^>]*>)?\s*\(\s*ws\(\s*["']\/v1\/sessions["']\s*\)/.test(text)
    ) {
      console.error(`single-session-create: ${rel} bypasses createManagedSession()`);
      failed = true;
    }
    if (/respond-async/i.test(text)) {
      console.error(`no-session-preparation-track: ${rel} opts into a parallel async create protocol`);
      failed = true;
    }
    if (/type=["']password["']/.test(text)) {
      console.error(`no-raw-secret-input: ${rel} hosts a raw password input — use <SecretField>`);
      failed = true;
    }
    if (legacyNotice.test(text)) {
      console.error(`shared-notice-owner: ${rel} must use InlineNotice`);
      failed = true;
    }
    const lines = surfaceLines(text);
    if (lines > MAX_LINES) {
      console.error(`file-size: ${rel} is ${lines} lines (> ${MAX_LINES}) — split the surface`);
      failed = true;
    }
  }
}

// Cause/effect table: L1 static/conditional legacy class or CSS recipe -> reject;
// L2 shared notice or contextual issue-banner spacing -> accept. The product
// owns placement and copy; it must not restore the deleted generic recipe.
const legacyNotice = /(?:className\s*=\s*(?:["']|\{)[^>\n]*?(?<![\w-])banner(?:[ "'`}]|$)|\.banner(?:[\s.{:]|$))/;
for (const [fixture, rejected] of [
  ['<div className="banner info">', true],
  ['<div className={warn ? "banner warn" : "banner info"}>', true],
  ['.banner.warn { color: red; }', true],
  ['<InlineNotice tone="warning" />', false],
  ['.issue-banner { margin-top: 10px; }', false],
  ['<InlineNotice className="issue-banner" tone="warning">', false],
]) assert.equal(legacyNotice.test(fixture), rejected);
for (const name of readdirSync(join(ROOT, "styles"))) {
  if (name.endsWith(".css") && legacyNotice.test(readFileSync(join(ROOT, "styles", name), "utf8"))) {
    console.error(`shared-notice-owner: styles/${name} restores a local banner recipe`);
    failed = true;
  }
}
walk(ROOT);
if (failed) process.exit(1);
console.log("OK - console lint passed.");
