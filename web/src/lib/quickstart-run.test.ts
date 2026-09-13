import { describe, expect, it } from "vitest";
import { BUILTIN_LOCAL_ENVIRONMENT_ID } from "./api/client";
import { STARTERS, starterOutcomeDraft } from "./agent-starters";
import { quickRunSessionRequest } from "./quickstart-run";

const base = {
  agentId: "release-reviewer",
  sourceRevision: 7,
  task: "Review the pending release.",
  title: "Quickstart first run",
};

describe("quickstart first run", () => {
  it("pins the Session to the revision the publish boundary acknowledged", () => {
    // A Session must never load an unversioned fallback configuration, so the
    // request carries the publication's revision rather than the draft's.
    expect(quickRunSessionRequest(base).agent).toEqual({
      id: "release-reviewer",
      type: "agent",
      version: 7,
    });
  });

  it("falls back to the built-in Environment only when none was chosen", () => {
    expect(quickRunSessionRequest(base).environment_id).toBe(BUILTIN_LOCAL_ENVIRONMENT_ID);
    expect(quickRunSessionRequest({ ...base, environmentId: "env_1" }).environment_id).toBe("env_1");
  });

  it("sends the task alone, or the task followed by one Outcome", () => {
    // Cause/effect decision table: R1 no Outcome -> exactly the first message,
    // so an ordinary run is unchanged; R2 an Outcome -> the same first message
    // and then the Outcome, in one atomic creation. Order matters: the grader
    // evaluates against the task, which must already be in the batch.
    expect(quickRunSessionRequest(base).initial_events).toEqual([
      { type: "user.message", content: [{ type: "text", text: "Review the pending release." }] },
    ]);
    const withOutcome = quickRunSessionRequest({
      ...base,
      outcome: { description: "A checked release note", rubric: "- names every risk", maxIterations: 4 },
    });
    expect(withOutcome.initial_events).toEqual([
      { type: "user.message", content: [{ type: "text", text: "Review the pending release." }] },
      {
        type: "user.define_outcome",
        description: "A checked release note",
        rubric: { type: "text", content: "- names every risk" },
        max_iterations: 4,
      },
    ]);
  });

  it("never admits more than the one Outcome creation accepts", () => {
    const events = quickRunSessionRequest({
      ...base,
      outcome: { description: "d", rubric: "r", maxIterations: 3 },
    }).initial_events ?? [];
    expect(events.filter((event) => event.type === "user.define_outcome")).toHaveLength(1);
  });
});

describe("starter outcome proposals", () => {
  it("projects a proposal into the authoring draft in the operator's language", () => {
    const starter = STARTERS.find((candidate) => candidate.id === "evidence-brief")!;
    const english = starterOutcomeDraft(starter, (en) => en);
    expect(english?.description).toBe(starter.outcome!.description);
    expect(english?.rubric).toBe(starter.outcome!.rubric);
    expect(starterOutcomeDraft(starter, (_en, zh) => zh)?.rubric).toBe(starter.outcome!.rubricZh);
  });

  it("proposes nothing for Starters whose value is not a graded deliverable", () => {
    const starter = STARTERS.find((candidate) => candidate.id === "task-assistant")!;
    expect(starterOutcomeDraft(starter, (en) => en)).toBeUndefined();
  });
});

it("first-run Vaults retain explicit order and do not change the published Agent", () => {
  // V1 default account selections -> ordered Session vault_ids; V2 absent ->
  // unchanged ordinary request; V3 publication identity always stays pinned.
  const request = quickRunSessionRequest({ ...base, vaultIds: ["vault-b", "vault-a"] });
  expect(request.vault_ids).toEqual(["vault-b", "vault-a"]);
  expect(request.agent).toEqual({ type: "agent", id: base.agentId, version: base.sourceRevision });
  expect(quickRunSessionRequest(base)).not.toHaveProperty("vault_ids");
});
