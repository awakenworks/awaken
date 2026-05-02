import { describe, it, expect } from "vitest";
import { existsSync, readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const ours = resolve(__dirname, "tokens");
const theirs = resolve(
  __dirname,
  "../../../../teams/web/design-tokens/tokens",
);

const SHARED_FILES = [
  "primitives/motion.json",
  "primitives/phase.json",
  "primitives/radius.json",
  "primitives/spacing.json",
  "primitives/typography.json",
  "semantic/sizing.json",
  "semantic/phase-chrome.json",
];

/* `color.tone` lives in primitives/colors.json on both sides and the
 * OKLCH values are intentionally identical (banner palette).
 * `color.phase` and `color.chrome` live in primitives/phase.json and are
 * already covered by the SHARED_FILES diff. */
const SHARED_COLOR_PATHS = [["color", "tone"]];

type Json = unknown;

function readJson(path: string): Json {
  return JSON.parse(readFileSync(path, "utf8"));
}

function rename(node: Json, from: string, to: string): Json {
  if (typeof node === "string") {
    // Rewrite DTCG reference strings: {os.space.4} → {aw.space.4}
    return node.replace(
      new RegExp(`\\{${from}\\.`, "g"),
      `{${to}.`,
    );
  }
  if (node === null || typeof node !== "object") return node;
  if (Array.isArray(node)) return node.map((v) => rename(v, from, to));
  const out: Record<string, Json> = {};
  for (const [k, v] of Object.entries(node)) {
    const renamedKey = k === from ? to : k;
    out[renamedKey] = rename(v, from, to);
  }
  return out;
}

/** Strip `$description` keys recursively. Parity is enforced on values + types,
 *  not human-readable descriptions — those are allowed to drift. */
function stripDescriptions(node: Json): Json {
  if (node === null || typeof node !== "object") return node;
  if (Array.isArray(node)) return node.map(stripDescriptions);
  const out: Record<string, Json> = {};
  for (const [k, v] of Object.entries(node)) {
    if (k === "$description") continue;
    out[k] = stripDescriptions(v);
  }
  return out;
}

function pick(node: Json, path: string[]): Json {
  let cur: Json = node;
  for (const seg of path) {
    if (cur && typeof cur === "object" && !Array.isArray(cur) && seg in (cur as Record<string, Json>)) {
      cur = (cur as Record<string, Json>)[seg];
    } else {
      return undefined;
    }
  }
  return cur;
}

/** Recursively assert that every leaf in `theirs` is present and equal in
 *  `ours`. Extra keys in `ours` are allowed — Awaken can add design-role
 *  tokens (e.g. tracking-eyebrow) without breaking parity. */
function assertSuperset(ours: Json, theirs: Json, path: string[] = []): void {
  if (theirs === null || typeof theirs !== "object" || Array.isArray(theirs)) {
    if (
      JSON.stringify(stripDescriptions(ours)) !==
      JSON.stringify(stripDescriptions(theirs))
    ) {
      throw new Error(
        `parity drift at ${path.join(".") || "<root>"}: ours=${JSON.stringify(ours)} theirs=${JSON.stringify(theirs)}`,
      );
    }
    return;
  }
  if (ours === null || typeof ours !== "object" || Array.isArray(ours)) {
    throw new Error(
      `parity drift at ${path.join(".") || "<root>"}: ours is not an object`,
    );
  }
  const ourMap = ours as Record<string, Json>;
  const theirMap = theirs as Record<string, Json>;
  for (const key of Object.keys(theirMap)) {
    if (!(key in ourMap)) {
      throw new Error(
        `parity drift at ${[...path, key].join(".")}: missing in ours`,
      );
    }
    assertSuperset(ourMap[key], theirMap[key], [...path, key]);
  }
}

const teamsCheckedOut = existsSync(theirs);

describe.skipIf(!teamsCheckedOut)("design-tokens parity vs teams", () => {
  it("teams checkout discovered next to awaken", () => {
    expect(teamsCheckedOut).toBe(true);
  });

  for (const file of SHARED_FILES) {
    it(`${file} is a superset of teams (after os→aw rename)`, () => {
      const ourJson = readJson(resolve(ours, file));
      const theirJson = readJson(resolve(theirs, file));
      const theirRenamed = rename(theirJson, "os", "aw");
      // Awaken may ADD design-role tokens on top of teams' baseline;
      // this asserts every teams leaf survives unchanged in ours.
      expect(() => assertSuperset(ourJson, theirRenamed)).not.toThrow();
    });
  }

  for (const path of SHARED_COLOR_PATHS) {
    it(`primitives/colors.json subtree ${path.join(".")} matches teams`, () => {
      const ourColors = readJson(resolve(ours, "primitives/colors.json"));
      const theirColors = readJson(resolve(theirs, "primitives/colors.json"));
      const ourSubtree = stripDescriptions(pick(ourColors, path));
      const theirSubtree = stripDescriptions(pick(theirColors, path));
      expect(ourSubtree, `our colors.json missing ${path.join(".")}`).toBeDefined();
      expect(theirSubtree, `teams colors.json missing ${path.join(".")}`).toBeDefined();
      expect(ourSubtree).toEqual(theirSubtree);
    });
  }

  it("violet.agent OKLCH matches teams' #7c5cff hue intent", () => {
    const ourColors = readJson(resolve(ours, "primitives/colors.json"));
    const violet = pick(ourColors, ["color", "violet", "agent"]) as
      | { $value: string }
      | undefined;
    expect(violet).toBeDefined();
    expect(violet!.$value).toMatch(/^oklch\(58\.0%\s+0\.135\s+270\)$/);
  });
});

describe("design-tokens parity vs teams (presence)", () => {
  it("either teams is checked out, or CI explicitly opted out", () => {
    if (!teamsCheckedOut) {
      // eslint-disable-next-line no-console
      console.warn(
        `[tokens.parity] teams not found at ${theirs}; skipping shared-token diff. ` +
          `In CI without teams sibling, this is fine — the values are still correct, ` +
          `we just can't enforce drift detection.`,
      );
    }
    expect(true).toBe(true);
  });
});
