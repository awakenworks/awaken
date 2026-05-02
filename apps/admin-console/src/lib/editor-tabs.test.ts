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

describe("tab badges", () => {
  const empty = {
    id: "x",
    model_id: "m",
    system_prompt: "",
  } as const;
  function badge(id: string, spec: Parameters<NonNullable<(typeof AGENT_EDITOR_TABS)[number]["badge"]>>[0]) {
    const tab = AGENT_EDITOR_TABS.find((t) => t.id === id);
    return tab?.badge?.(spec) ?? null;
  }

  it("Tools badge: null when no allowed/excluded", () => {
    expect(badge("tools", { ...empty })).toBeNull();
  });

  it("Tools badge: allowed count alone", () => {
    expect(badge("tools", { ...empty, allowed_tools: ["a", "b", "c"] })).toBe("3");
  });

  it("Tools badge: includes excluded count when present", () => {
    expect(
      badge("tools", { ...empty, allowed_tools: ["a", "b"], excluded_tools: ["x"] }),
    ).toBe("2·−1");
  });

  it("Plugins badge: count when set", () => {
    expect(badge("plugins", { ...empty, plugin_ids: ["p1", "p2"] })).toBe("2");
    expect(badge("plugins", { ...empty })).toBeNull();
  });

  it("Delegates badge: count when set", () => {
    expect(badge("delegates", { ...empty, delegates: ["d1"] })).toBe("1");
    expect(badge("delegates", { ...empty })).toBeNull();
  });

  it("Basics + Advanced + History have no badge function", () => {
    expect(AGENT_EDITOR_TABS.find((t) => t.id === "basics")?.badge).toBeUndefined();
    expect(AGENT_EDITOR_TABS.find((t) => t.id === "advanced")?.badge).toBeUndefined();
    expect(AGENT_EDITOR_TABS.find((t) => t.id === "history")?.badge).toBeUndefined();
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
