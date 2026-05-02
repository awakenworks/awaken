import { describe, expect, it } from "vitest";
import {
  AGENT_EDITOR_TABS,
  DEFAULT_AGENT_EDITOR_TAB,
  isAgentEditorTab,
  readTabFromSearch,
  writeTabToSearch,
} from "./editor-tabs";

describe("AGENT_EDITOR_TABS", () => {
  it("exposes a non-empty list and includes the default", () => {
    expect(AGENT_EDITOR_TABS.length).toBeGreaterThan(0);
    expect(AGENT_EDITOR_TABS.map((t) => t.id)).toContain(DEFAULT_AGENT_EDITOR_TAB);
  });

  it("uses unique tab ids", () => {
    const ids = AGENT_EDITOR_TABS.map((t) => t.id);
    expect(new Set(ids).size).toBe(ids.length);
  });
});

describe("isAgentEditorTab", () => {
  it("accepts known tab ids", () => {
    expect(isAgentEditorTab("basics")).toBe(true);
    expect(isAgentEditorTab("advanced")).toBe(true);
    expect(isAgentEditorTab("history")).toBe(true);
  });

  it("rejects everything else", () => {
    expect(isAgentEditorTab("foo")).toBe(false);
    expect(isAgentEditorTab(undefined)).toBe(false);
    expect(isAgentEditorTab(null)).toBe(false);
    expect(isAgentEditorTab(42)).toBe(false);
  });
});

describe("readTabFromSearch", () => {
  it("returns the default when ?tab is absent", () => {
    expect(readTabFromSearch("")).toBe(DEFAULT_AGENT_EDITOR_TAB);
  });

  it("returns the default when ?tab is unrecognised", () => {
    expect(readTabFromSearch("?tab=bogus")).toBe(DEFAULT_AGENT_EDITOR_TAB);
  });

  it("returns the requested tab when present and valid", () => {
    expect(readTabFromSearch("?tab=tools")).toBe("tools");
  });

  it("accepts URLSearchParams instances directly", () => {
    expect(readTabFromSearch(new URLSearchParams({ tab: "delegates" }))).toBe(
      "delegates",
    );
  });
});

describe("writeTabToSearch", () => {
  it("removes the parameter when writing the default tab", () => {
    expect(
      writeTabToSearch("?tab=tools&other=keep", DEFAULT_AGENT_EDITOR_TAB).toString(),
    ).toBe("other=keep");
  });

  it("sets the parameter when writing a non-default tab", () => {
    expect(writeTabToSearch("", "advanced").toString()).toBe("tab=advanced");
  });

  it("preserves unrelated parameters", () => {
    expect(
      writeTabToSearch("?other=keep", "tools").toString(),
    ).toBe("other=keep&tab=tools");
  });
});
