import { describe, expect, it } from "vitest";
import { workspaceLabel } from "../app-state";
import { visibleNavigation } from "./paths";

// Hosted navigation cause/effect decision table:
// T1 C1 BYOK enabled -> E1 expose Providers & models plus Inference credentials.
// T2 !C1 -> E2 expose the model catalog as Models under Build, remove credentials
// and leave no tenant-facing AI Supply group.
// Workspace label partition:
// T3 default -> E3 friendly Default; T4 hosted personal coordinate -> E4 Personal
// Workspace; T5 short authored name -> E5 preserve; T6 long opaque coordinate ->
// E6 bounded, recognizable prefix/suffix while the full value remains in title.
describe("deployment-aware navigation", () => {
  it("retains local supply authoring when BYOK is enabled", () => {
    const supply = visibleNavigation(true).filter((item) => item.group === "supply");
    expect(supply.map(({ key, label }) => ({ key, label }))).toEqual([
      { key: "models", label: "Providers & models" },
      { key: "credentials", label: "Inference credentials" },
    ]);
  });

  it("projects only the Cloud model catalog when BYOK is disabled", () => {
    const navigation = visibleNavigation(false);
    expect(navigation.filter((item) => item.group === "supply")).toEqual([]);
    expect(navigation.find((item) => item.key === "models")).toMatchObject({
      label: "Models",
      group: "build",
    });
  });
});

describe("workspaceLabel", () => {
  it("bounds opaque coordinates without changing authored short names", () => {
    expect(workspaceLabel("default")).toBe("Default");
    expect(workspaceLabel("awaken:personal:acct_0123456789")).toBe("Personal Workspace");
    expect(workspaceLabel("product-design")).toBe("product-design");
    expect(workspaceLabel("workspace_abcdefghijklmnopqrstuvwxyz_0123456789")).toBe(
      "workspace_ab…23456789",
    );
  });
});
