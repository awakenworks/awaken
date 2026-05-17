#!/usr/bin/env node
/**
 * Catch drift between apps/www/src/styles/shiki/awaken.json (hand-maintained
 * hex approximations) and the canonical --aw-syntax-* / --aw-code-* OKLCH
 * values in packages/design-tokens/tokens/semantic/colors-light.json.
 *
 * Strategy: each tokenColor in the theme JSON carries a `_token` annotation
 * like  `"--aw-syntax-keyword (oklch 76% 0.140 270)"`. We parse that string
 * and compare it to the actual `$value` in colors-light.json. If someone
 * bumps the source token but forgets to update the theme annotation
 * (and the corresponding hex), this fails — forcing a deliberate resync.
 *
 * This script does not enforce numeric oklch→hex accuracy; that's the
 * maintainer's call when they edit the hex. The annotation contract is
 * what's enforced.
 */

import fs from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const pkgRoot = path.resolve(__dirname, "..");
const repoRoot = path.resolve(pkgRoot, "../..");

const themePath = path.join(repoRoot, "apps/www/src/styles/shiki/awaken.json");
const colorsPath = path.join(pkgRoot, "tokens/semantic/colors-light.json");

const TOKEN_MAP = {
  "--aw-syntax-comment": ["aw", "syntax", "comment"],
  "--aw-syntax-keyword": ["aw", "syntax", "keyword"],
  "--aw-syntax-type":    ["aw", "syntax", "type"],
  "--aw-syntax-string":  ["aw", "syntax", "string"],
  "--aw-syntax-fn":      ["aw", "syntax", "fn"],
  "--aw-syntax-num":     ["aw", "syntax", "num"],
  "--aw-code-fg":        ["aw", "code-fg"],
  "--aw-code-bg":        ["aw", "code-bg"],
};

const theme = JSON.parse(await fs.readFile(themePath, "utf8"));
const colors = JSON.parse(await fs.readFile(colorsPath, "utf8"));

function pick(obj, p) {
  let cur = obj;
  for (const seg of p) {
    if (cur && typeof cur === "object" && seg in cur) cur = cur[seg];
    else return undefined;
  }
  return cur;
}

function normalizeOklch(s) {
  // Accept variations like "oklch(76% 0.140 270)" / "oklch(76.0% 0.14 270)".
  const m = s.match(/oklch\(\s*([\d.]+)%?\s+([\d.]+)\s+([\d.]+)\s*\)/);
  if (!m) return null;
  return `${+m[1]} ${+m[2]} ${+m[3]}`;
}

const errors = [];
let checked = 0;

for (const tc of theme.tokenColors ?? []) {
  const annot = tc._token ?? "";
  const m = annot.match(/(--aw-[a-z-]+)\s*\(oklch\s+([\d.]+)%\s+([\d.]+)\s+([\d.]+)\)/);
  if (!m) continue;
  const [, tokenName, l, c, h] = m;
  const tokenPath = TOKEN_MAP[tokenName];
  if (!tokenPath) {
    errors.push(`unknown token "${tokenName}" in theme _token annotation`);
    continue;
  }
  const sourceToken = pick(colors, tokenPath);
  if (!sourceToken || typeof sourceToken !== "object" || !("$value" in sourceToken)) {
    errors.push(`token path "${tokenPath.join(".")}" missing or malformed in colors-light.json`);
    continue;
  }
  const expected = `${+l} ${+c} ${+h}`;
  const actual = normalizeOklch(sourceToken.$value);
  if (actual !== expected) {
    errors.push(
      `drift for ${tokenName}: theme annotation says oklch(${expected}) ` +
      `but colors-light.json has "${sourceToken.$value}"`,
    );
  }
  checked++;
}

if (errors.length > 0) {
  console.error("✘ Shiki theme drift detected:");
  for (const e of errors) console.error(`    ${e}`);
  console.error("");
  console.error("Fix: edit apps/www/src/styles/shiki/awaken.json — update both");
  console.error("the hex value (re-approximating from the new oklch) AND the");
  console.error("_token annotation so this validator agrees.");
  process.exit(1);
}

console.log(`✔ Shiki theme annotations match tokens (${checked} checked)`);
