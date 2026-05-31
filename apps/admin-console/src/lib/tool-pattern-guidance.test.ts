import { describe, expect, it } from "vitest";
import {
  normalizeToolPatternInput,
  toolPatternDescription,
  toolPatternExamples,
  toolPatternPlaceholder,
  validateToolPatternInput,
} from "./tool-pattern-guidance";

describe("tool pattern guidance", () => {
  it("centralizes examples for id and call pattern fields", () => {
    expect(toolPatternPlaceholder("tool-id")).toBe("mcp__github__*");
    expect(toolPatternPlaceholder("tool-call")).toBe("Bash(npm *)");
    expect(toolPatternExamples("tool-id")).toContain("mcp__github__*");
    expect(toolPatternExamples("tool-call")).toContain('Edit(file_path ~ "src/**")');
    expect(toolPatternDescription("tool-id")).toContain("tool ids only");
  });

  it("normalizes surrounding whitespace without changing inner match syntax", () => {
    expect(normalizeToolPatternInput(' Bash(command ~ "rm *") ')).toBe('Bash(command ~ "rm *")');
  });

  it("validates common pattern mistakes consistently", () => {
    expect(validateToolPatternInput("", { kind: "tool-id" })).toBeNull();
    expect(validateToolPatternInput("", { kind: "tool-id", allowEmpty: false })).toContain("Enter");
    expect(validateToolPatternInput("foo\\", { kind: "tool-id" })).toContain("dangling escape");
    expect(validateToolPatternInput("Bash(npm *)", { kind: "tool-id" })).toContain(
      "Tool ID patterns",
    );
    expect(validateToolPatternInput("Bash(npm *)", { kind: "tool-call" })).toBeNull();
  });
});
