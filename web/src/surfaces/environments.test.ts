import { describe, expect, it } from "vitest";
import { isolationLabel, runtimeLabel } from "./environments";

describe("runtimeLabel", () => {
  it("separates native runtime from placement", () => {
    expect(runtimeLabel()).toBe("Native");
    expect(runtimeLabel("awaken")).toBe("Native");
  });

  it("makes ACP adapters explicit", () => {
    expect(runtimeLabel("acp:claude")).toBe("Claude Code · ACP");
    expect(runtimeLabel("acp:custom-agent")).toBe("custom-agent · ACP");
  });
});

describe("isolationLabel", () => {
  it("makes persisted sandbox containment visible in the list", () => {
    expect(isolationLabel({
      type: "cloud",
      sandbox: { isolation: "namespace", network: { mode: "none" } },
    })).toBe("namespace · no egress");
  });

  it("shows ordinary environment networking without a sandbox", () => {
    expect(isolationLabel({ type: "cloud", networking: { type: "unrestricted" } })).toBe("unrestricted");
  });
});
