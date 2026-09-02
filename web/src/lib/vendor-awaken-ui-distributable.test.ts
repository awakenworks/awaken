import { existsSync, readFileSync } from "node:fs";
import { SuiteSwitcher } from "@awaken/ui";
import { describe, expect, it } from "vitest";

const runtimeIndexUrl = new URL("../../vendor/awaken-ui/dist/index.js", import.meta.url);
const typeIndexUrl = new URL("../../vendor/awaken-ui/dist/index.d.ts", import.meta.url);
const relativeExport = /^\s*export(?:\s+type)?\s+(?:\*|\{[^}]*\})\s+from\s+["'](\.[^"']+\.js)["'];?/gm;

function relativeExportTargets(source: string): string[] {
  return [...source.matchAll(relativeExport)].map((match) => match[1]);
}

function missingRelativeExports(source: string, indexUrl: URL, types: boolean): string[] {
  return relativeExportTargets(source).filter((target) => {
    const resolved = types ? target.replace(/\.js$/, ".d.ts") : target;
    return !existsSync(new URL(resolved, indexUrl));
  });
}

describe("vendored @awaken/ui distributable closure", () => {
  /**
   * Cause/effect table: C1 the runtime or type index exports a relative module;
   * C2 the corresponding `.js` or `.d.ts` target is present; C3 the workspace
   * consumer resolves that same package. E1=C1+C2+C3 closes both source and
   * installed projections; E2=C1+missing target reports that exact export.
   *
   * | Rule | index | target | effect |
   * | V1 | runtime | present + installed | E1 |
   * | V2 | types | present + installed | E1 |
   * | V3 | runtime | missing | E2 |
   * | V4 | types | missing | E2 |
   *
   * The index owns the public export inventory; this test only verifies its
   * filesystem closure and does not create a second component catalogue.
   */
  it("resolves every relative runtime and type export", () => {
    const runtimeIndex = readFileSync(runtimeIndexUrl, "utf8");
    const typeIndex = readFileSync(typeIndexUrl, "utf8");
    const suiteSwitcher = "./navigation/suite-switcher.js";

    expect(relativeExportTargets(runtimeIndex)).toContain(suiteSwitcher);
    expect(relativeExportTargets(typeIndex)).toContain(suiteSwitcher);
    expect(SuiteSwitcher).toBeTypeOf("function");
    expect(missingRelativeExports(runtimeIndex, runtimeIndexUrl, false)).toEqual([]); // V1
    expect(missingRelativeExports(typeIndex, typeIndexUrl, true)).toEqual([]); // V2

    const missing = 'export { Missing } from "./navigation/not-present.js";';
    expect(missingRelativeExports(missing, runtimeIndexUrl, false)).toEqual([
      "./navigation/not-present.js",
    ]); // V3
    expect(missingRelativeExports(missing, typeIndexUrl, true)).toEqual([
      "./navigation/not-present.js",
    ]); // V4
  });
});
