#!/usr/bin/env node
/**
 * Walks every JSON file under tokens/ and asserts every leaf has a $value
 * and a $type. Catches typos like `value` (no $) or missing types early.
 *
 * Mirror of teams/web/design-tokens/scripts/validate.mjs.
 */
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(__dirname, "../tokens");

const ALLOWED_TYPES = new Set([
  "color",
  "dimension",
  "shadow",
  "fontFamily",
  "fontWeight",
  "duration",
  "cubicBezier",
  "number",
]);

let errors = 0;
const log = (file, msg) => {
  console.error(`✘ ${file}: ${msg}`);
  errors++;
};

const isLeaf = (obj) =>
  obj && typeof obj === "object" && ("$value" in obj || "value" in obj);

const walk = (file, node, pathParts = []) => {
  if (isLeaf(node)) {
    if (!("$value" in node)) {
      log(file, `${pathParts.join(".")} missing $value (got 'value' — DTCG requires '$' prefix)`);
    }
    if (!("$type" in node)) {
      log(file, `${pathParts.join(".")} missing $type`);
    } else if (!ALLOWED_TYPES.has(node.$type)) {
      log(file, `${pathParts.join(".")} unknown $type "${node.$type}"`);
    }
    return;
  }
  if (node && typeof node === "object") {
    for (const [k, v] of Object.entries(node)) {
      // Documentation keys (not DTCG tokens).
      if (k.startsWith("_")) continue;
      walk(file, v, [...pathParts, k]);
    }
  }
};

const eachFile = (dir) => {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, entry.name);
    if (entry.isDirectory()) eachFile(p);
    else if (entry.isFile() && entry.name.endsWith(".json")) {
      try {
        const json = JSON.parse(fs.readFileSync(p, "utf8"));
        walk(path.relative(root, p), json);
      } catch (e) {
        log(path.relative(root, p), `parse error: ${e.message}`);
      }
    }
  }
};

eachFile(root);
if (errors > 0) {
  console.error(`\nvalidate: ${errors} issue(s)`);
  process.exit(1);
}
console.log("validate: ok");
