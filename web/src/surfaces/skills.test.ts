import { describe, expect, it } from "vitest";
import { skillNeedsSandbox, skillRuntimeFromTar } from "./skills";

function tar(entries: Array<[string, string, number?]>): Uint8Array {
  const encoder = new TextEncoder();
  const chunks = entries.map(([path, content, mode = 0o644]) => {
    const body = encoder.encode(content);
    const chunk = new Uint8Array(512 + Math.ceil(body.length / 512) * 512);
    chunk.set(encoder.encode(path), 0);
    chunk.set(encoder.encode(`${mode.toString(8).padStart(7, "0")}\0`), 100);
    chunk.set(encoder.encode(`${body.length.toString(8).padStart(11, "0")}\0`), 124);
    chunk[156] = "0".charCodeAt(0);
    chunk.set(body, 512);
    return chunk;
  });
  const size = chunks.reduce((total, chunk) => total + chunk.length, 1024);
  const archive = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    archive.set(chunk, offset);
    offset += chunk.length;
  }
  return archive;
}

describe("Skill Sandbox requirement", () => {
  it("keeps a single instruction-only SKILL.md in the Brain", () => {
    expect(skillNeedsSandbox(["SKILL.md"], "---\nenvironment: instruction-only\n---\nThink.")).toBe(false);
  });

  it("requires a Sandbox for an explicit filesystem declaration or supporting files", () => {
    expect(skillNeedsSandbox(["SKILL.md"], "---\nenvironment: filesystem\n---\nRead files.")).toBe(true);
    expect(skillNeedsSandbox(["SKILL.md", "scripts/check.sh"], "Think.")).toBe(true);
  });

  it("derives runtime needs from the official beta content archive", () => {
    expect(skillRuntimeFromTar(tar([["review/SKILL.md", "---\nenvironment: instruction-only\n---\nThink."]]))).toBe(false);
    expect(skillRuntimeFromTar(tar([
      ["review/SKILL.md", "---\nenvironment: instruction-only\n---\nCheck."],
      ["review/scripts/check.sh", "#!/bin/sh\nexit 0\n", 0o755],
    ]))).toBe(true);
    expect(() => skillRuntimeFromTar(new Uint8Array(512))).toThrow(/no SKILL\.md/);
  });
});
