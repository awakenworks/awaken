import StyleDictionary from "style-dictionary";
import { fileURLToPath } from "node:url";
import path from "node:path";
import fs from "node:fs/promises";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(__dirname, "..");
const out = path.resolve(root, "dist/css");

/* DTCG-aware transforms (Style Dictionary v4 API: `filter` + `transform`).
 * Each $type gets serialized to its canonical CSS form.
 *
 * Schema mirrors teams/web/design-tokens (intentional — see
 * design-tokens/README.md for the why). */

const stringPassthrough = (kinds) => ({
  type: "value",
  transitive: true,
  filter: (token) => kinds.includes(token.$type ?? token.type),
  transform: (token) => String(token.$value ?? token.value),
});

StyleDictionary.registerTransform({ name: "dtcg/color/css",      ...stringPassthrough(["color"]) });
StyleDictionary.registerTransform({ name: "dtcg/shadow/css",     ...stringPassthrough(["shadow"]) });
StyleDictionary.registerTransform({ name: "dtcg/duration/css",   ...stringPassthrough(["duration"]) });
StyleDictionary.registerTransform({ name: "dtcg/fontWeight/css", ...stringPassthrough(["fontWeight"]) });
StyleDictionary.registerTransform({ name: "dtcg/number/css",     ...stringPassthrough(["number"]) });

StyleDictionary.registerTransform({
  name: "dtcg/dimension/css",
  type: "value",
  transitive: true,
  filter: (token) => (token.$type ?? token.type) === "dimension",
  transform: (token) => {
    const v = token.$value ?? token.value;
    return typeof v === "number" ? `${v}px` : String(v);
  },
});

StyleDictionary.registerTransform({
  name: "dtcg/cubicBezier/css",
  type: "value",
  transitive: true,
  filter: (token) => (token.$type ?? token.type) === "cubicBezier",
  transform: (token) => {
    const v = token.$value ?? token.value;
    if (Array.isArray(v) && v.length === 4) return `cubic-bezier(${v.join(", ")})`;
    return String(v);
  },
});

StyleDictionary.registerTransform({
  name: "dtcg/fontFamily/css",
  type: "value",
  transitive: true,
  filter: (token) => (token.$type ?? token.type) === "fontFamily",
  transform: (token) => {
    const v = token.$value ?? token.value;
    const arr = Array.isArray(v) ? v : [v];
    return arr.map((f) => (/\s/.test(f) && !/^['"]/.test(f) ? `'${f}'` : f)).join(", ");
  },
});

StyleDictionary.registerTransformGroup({
  name: "dtcg-css",
  transforms: [
    "attribute/cti",
    "name/kebab",
    "dtcg/color/css",
    "dtcg/dimension/css",
    "dtcg/shadow/css",
    "dtcg/cubicBezier/css",
    "dtcg/duration/css",
    "dtcg/fontFamily/css",
    "dtcg/fontWeight/css",
    "dtcg/number/css",
  ],
});

const log = { verbosity: "verbose" };

const lightConfig = {
  log,
  source: [
    `${root}/tokens/primitives/**/*.json`,
    `${root}/tokens/semantic/sizing.json`,
    `${root}/tokens/semantic/colors-light.json`,
    `${root}/tokens/semantic/phase-chrome.json`,
  ],
  platforms: {
    css: {
      transformGroup: "dtcg-css",
      buildPath: `${out}/`,
      files: [
        {
          destination: "tokens.css",
          format: "css/variables",
          /* Only emit tokens in the public `aw.*` namespace. Color primitives
           * (`color.*` in primitives/colors.json and primitives/phase.json) are
           * indirection-only and would otherwise leak as --color-*. Other
           * primitive files (motion, radius, shadows, spacing, typography) are
           * already in aw.* and ride through. */
          filter: (token) => Array.isArray(token.path) && token.path[0] === "aw",
          options: { selector: ":root, [data-theme=\"light\"]", outputReferences: false },
        },
      ],
    },
    json: {
      transformGroup: "dtcg-css",
      buildPath: `${out}/`,
      files: [{ destination: "tokens.json", format: "json/flat" }],
    },
  },
};

const darkConfig = {
  log,
  source: [
    `${root}/tokens/primitives/**/*.json`,
    `${root}/tokens/semantic/colors-dark.json`,
  ],
  platforms: {
    css: {
      transformGroup: "dtcg-css",
      buildPath: `${out}/`,
      files: [
        {
          destination: "tokens-dark.css",
          format: "css/variables",
          filter: (token) => (token.filePath || "").includes("/semantic/colors-dark.json"),
          options: { selector: "[data-theme=\"dark\"]", outputReferences: false },
        },
      ],
    },
  },
};

const sdLight = new StyleDictionary(lightConfig);
await sdLight.buildAllPlatforms();

const sdDark = new StyleDictionary(darkConfig);
await sdDark.buildAllPlatforms();

const darkPath = `${out}/tokens-dark.css`;
const darkSrc = await fs.readFile(darkPath, "utf8");
const innerStart = darkSrc.indexOf("[data-theme=\"dark\"] {") + "[data-theme=\"dark\"] {".length;
const innerEnd = darkSrc.lastIndexOf("}");
const decls = darkSrc.slice(innerStart, innerEnd).trim();
const autoDark = `/**
 * Do not edit directly, this file was auto-generated from the dark
 * token block, scoped to system colour-scheme preference.
 */

@media (prefers-color-scheme: dark) {
  :root:not([data-theme]) {
${decls
  .split("\n")
  .map((l) => (l.trim() ? `  ${l}` : ""))
  .join("\n")}
  }
}
`;
await fs.writeFile(`${out}/tokens-auto-dark.css`, autoDark);
console.log(`✔︎ ${out}/tokens-auto-dark.css`);
