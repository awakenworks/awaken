import { describe, expect, it } from "vitest";
import { deriveReadiness, runtimeStatus } from "./readiness";

// Cause/effect graph:
// C1=runnable Provider model, C2=ready ACP, C3=published Agent,
// C4=native Runtime, C5=Environment, C6=Cloud-managed deployment. Effects are the three independently owned
// readiness rows. Constraints: supply is C1∨C2; execution is C2∨C4∨C5; Agent is C3.
// Decision rules: R1 all absent -> actionable supply/Agent + attention execution;
// R2 Provider+Agent+native -> all ready; R3 ACP+Agent only -> all ready without
// inventing Provider/Environment requirements; R4 detected ACP without login ->
// not ready, while an available login is ready; R5 managed Cloud without a
// catalogue is attention (an Operations fault), never a tenant setup action;
// R6 split Control without Managed runtime -> omit the execution row and never
// probe or suggest an unmounted Environment surface.
describe("Workspace readiness decision table", () => {
  const empty = {
    workspace: "default",
    models: 0,
    providerConnections: 0,
    acp: 0,
    publishedAgents: 0,
    environments: 0,
    nativeRuntime: false,
    managedModels: false,
    managedRuntime: true,
  };

  it("R1 exposes the owning remediation for every missing fact", () => {
    expect(deriveReadiness(empty).map(({ status, href }) => [status, href])).toEqual([
      ["action", "/w/default/models"],
      ["action", "/w/default/agents"],
      ["attention", "/w/default/environments"],
    ]);
  });

  it("R2 and R3 accept either complete Provider or ACP execution paths", () => {
    expect(deriveReadiness({ ...empty, models: 2, providerConnections: 1, publishedAgents: 1, nativeRuntime: true })
      .every((item) => item.status === "ready")).toBe(true);
    expect(deriveReadiness({ ...empty, acp: 1, publishedAgents: 1 })
      .every((item) => item.status === "ready")).toBe(true);
  });

  it("R4 treats installed and authenticated ACP observations separately", () => {
    const runtime = { id: "acp:claude", label: "Claude", kind: "acp" as const, description: "" };
    expect(runtimeStatus(runtime)).toBe("not_detected");
    expect(runtimeStatus({ ...runtime, local: { detected: true, login_state: "login_required" } }))
      .toBe("login_required");
    expect(runtimeStatus({ ...runtime, local: { detected: true, login_state: "available" } }))
      .toBe("ready");
  });

  it("R5 does not ask Cloud tenants to configure AI supply", () => {
    expect(deriveReadiness({ ...empty, managedModels: true })[0]).toMatchObject({
      label: "Models",
      detail: "Cloud model catalog is temporarily unavailable",
      status: "attention",
    });
    expect(deriveReadiness({ ...empty, managedModels: true, models: 2 })[0]).toMatchObject({
      detail: "2 Cloud-managed models available",
      status: "ready",
    });
  });

  it("R6 omits execution readiness when the process does not own Managed runtime", () => {
    expect(deriveReadiness({ ...empty, managedRuntime: false }).map((item) => item.id)).toEqual([
      "supply",
      "agent",
    ]);
  });
});
