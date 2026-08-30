import { describe, expect, it } from "vitest";
import { NAV } from "../../lib/navigation/paths";
import { navItemMatchesQuery, resetRouteScroll } from "./AppShell";

describe("command palette information architecture", () => {
  const sessions = NAV.find((item) => item.key === "sessions")!;
  const models = NAV.find((item) => item.key === "models")!;

  it("finds pages through localized page and workflow language", () => {
    expect(navItemMatchesQuery(sessions, "sessions")).toBe(true);
    expect(navItemMatchesQuery(sessions, "会话")).toBe(true);
    expect(navItemMatchesQuery(sessions, "运行")).toBe(true);
    expect(navItemMatchesQuery(models, "连接")).toBe(true);
    expect(navItemMatchesQuery(models, "not-a-page")).toBe(false);
  });

  it("resets the shell-owned scroll container when navigation changes", () => {
    const content = { scrollTop: 720 };
    resetRouteScroll(content);
    expect(content.scrollTop).toBe(0);
    expect(() => resetRouteScroll(null)).not.toThrow();
  });
});
