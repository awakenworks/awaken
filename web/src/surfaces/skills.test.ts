import { describe, expect, it } from "vitest";
import { skillNeedsSandbox } from "./skills";

describe("Skill Sandbox requirement", () => {
  it("keeps a single instruction-only SKILL.md in the Brain", () => {
    expect(skillNeedsSandbox(["SKILL.md"], "---\nenvironment: instruction-only\n---\nThink.")).toBe(false);
  });

  it("requires a Sandbox for an explicit filesystem declaration or supporting files", () => {
    expect(skillNeedsSandbox(["SKILL.md"], "---\nenvironment: filesystem\n---\nRead files.")).toBe(true);
    expect(skillNeedsSandbox(["SKILL.md", "scripts/check.sh"], "Think.")).toBe(true);
  });
});
