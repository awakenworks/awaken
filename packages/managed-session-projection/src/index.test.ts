import { describe, expect, it } from "vitest";
import {
  EMPTY_LIVE_PREVIEW,
  MANAGED_SESSION_EVENT_TYPES,
  MANAGED_SESSION_PREVIEW_TYPES,
  ManagedProjectionConflictError,
  countCommittedEventUpdates,
  isCommittedStreamEvent,
  isManagedSessionActiveStatus,
  managedSessionAdmission,
  managedSessionPresentationPhase,
  managedSessionStatusPresentation,
  mergeCommittedEvents,
  projectManagedSessionRuntime,
  reduceLivePreview,
  type ManagedEvent,
  type ManagedStreamEvent,
} from "./index.js";

const event = (value: Record<string, unknown>): ManagedEvent => value as unknown as ManagedEvent;
const stream = (value: Record<string, unknown>): ManagedStreamEvent => value as unknown as ManagedStreamEvent;

describe("Managed Session committed projection decision table", () => {
  /**
   * Cause/effect rules:
   * R0: C0 the current SDK oracle adds/removes committed or preview types ->
   * E0 both subscription classes change from the generated catalog.
   * Constraint: the classifier rejects every generated preview envelope and
   * admits an ID-bearing generated committed event; there is no local list.
   */
  it("classifies the oracle-generated wire catalog", () => {
    expect(MANAGED_SESSION_EVENT_TYPES).toContain("session.status_terminated");
    expect(MANAGED_SESSION_EVENT_TYPES).toContain("user.tool_result");
    expect(MANAGED_SESSION_PREVIEW_TYPES).toEqual(["event_delta", "event_start"]);
    for (const type of MANAGED_SESSION_PREVIEW_TYPES) {
      expect(isCommittedStreamEvent(stream({ type, id: "misleading" }))).toBe(false);
    }
    expect(isCommittedStreamEvent(stream({ id: "idle", type: "session.status_idle" }))).toBe(true);
  });

  /**
   * Cause/effect graph: C1 same/different Event id; C2 current/incoming
   * processed_at is U(absent|null/empty) or P(nonempty); C3 immutable JSON is
   * equal. Effects: E1 keep first-seen position/material; E2 enrich only U->P;
   * E3 retain the existing P under delayed U. Decision table:
   * | Rule | id | current | incoming | immutable | effect |
   * | M1 | new | any | any | n/a | append |
   * | M2 | same | U | U | equal | first |
   * | M3 | same | U | P | equal | first + P |
   * | M4 | same | P | U | equal | first + P |
   * | M5 | same | P | same P | equal | first |
   */
  it("merges identity overlap with monotonic processed_at in either arrival order", () => {
    const unprocessed = event({ id: "reply", type: "user.tool_confirmation", tool_use_id: "tool", result: "allow" });
    const nullProcessed = event({ ...unprocessed, processed_at: null });
    const processed = event({ ...unprocessed, processed_at: "2026-08-29T00:00:00Z" });
    const status = event({ id: "idle", type: "session.status_idle", processed_at: "2026-08-29T00:00:01Z", stop_reason: { type: "end_turn" } });
    const unprocessedHistory = [unprocessed];
    const processedHistory = [processed];

    expect(mergeCommittedEvents(unprocessedHistory, [nullProcessed])).toBe(unprocessedHistory);
    expect(mergeCommittedEvents(unprocessedHistory, [processed])[0]?.processed_at).toBe("2026-08-29T00:00:00Z");
    expect(mergeCommittedEvents(processedHistory, [unprocessed])).toBe(processedHistory);
    expect(mergeCommittedEvents(processedHistory, [processed])).toBe(processedHistory);
    expect(mergeCommittedEvents([unprocessed], [processed, status]).map(({ id }) => id)).toEqual(["reply", "idle"]);
    expect(mergeCommittedEvents([processed], [unprocessed])[0]?.processed_at)
      .toBe(mergeCommittedEvents([unprocessed], [processed])[0]?.processed_at);
    expect(countCommittedEventUpdates([unprocessed], [processed, status, status])).toBe(2);
  });

  /**
   * Cause/effect rules: C1 same id with changed type/payload/nested material or
   * two different P timestamps -> E1 throw a typed conflict before returning a
   * projection. Top-level undefined equals absence because both are the same
   * JSON default; null remains explicit. Only top-level processed_at is mutable.
   */
  it("fails closed on immutable or post-commit timestamp conflicts", () => {
    const allow = event({
      id: "reply",
      type: "user.tool_confirmation",
      tool_use_id: "tool",
      result: "allow",
      processed_at: "2026-08-29T00:00:00Z",
      detail: { processed_at: "nested-a" },
    });
    const absentOptional = event({ id: "message", type: "agent.message", content: [] });
    const undefinedOptional = event({ id: "message", type: "agent.message", content: [], optional: undefined });
    const current = [absentOptional];

    expect(mergeCommittedEvents(current, [undefinedOptional])).toBe(current);
    expect(() => mergeCommittedEvents([allow], [event({ ...allow, result: "deny" })]))
      .toThrow(ManagedProjectionConflictError);
    expect(() => mergeCommittedEvents([allow], [event({ ...allow, type: "user.tool_result" })]))
      .toThrow(ManagedProjectionConflictError);
    expect(() => mergeCommittedEvents([allow], [event({ ...allow, detail: { processed_at: "nested-b" } })]))
      .toThrow(ManagedProjectionConflictError);
    expect(() => mergeCommittedEvents([allow], [event({ ...allow, processed_at: "2026-08-29T00:00:01Z" })]))
      .toThrow(/processed_at changed after commit/);
    expect(() => mergeCommittedEvents(
      [event({ id: "tool", type: "agent.tool_use", name: "read", input: { path: "a" } })],
      [event({ id: "tool", type: "agent.tool_use", name: "write", input: { path: "b" } })],
    )).toThrow(/payload changed/);
    expect(() => mergeCommittedEvents(
      [event({ id: "result", type: "agent.tool_result", tool_use_id: "tool", is_error: false })],
      [event({ id: "result", type: "agent.tool_result", tool_use_id: "tool", is_error: true })],
    )).toThrow(/payload changed/);
    expect(() => mergeCommittedEvents(
      [event({ id: "nullable", type: "agent.message", content: [], session_thread_id: null })],
      [event({ id: "nullable", type: "agent.message", content: [] })],
    )).toThrow(/payload changed/);
    expect(() => mergeCommittedEvents(
      [event({ id: "array-null", type: "agent.message", content: [null] })],
      [event({ id: "array-null", type: "agent.message", content: [undefined] })],
    )).toThrow(/payload changed/);
  });
});

describe("Managed Session Runtime and admission decision table", () => {
  /**
   * Cause/effect graph: C1 lifecycle is running/rescheduled/idle/error;
   * C2 idle reason has an exact requires_action set or a non-gating reason.
   * Effects: E1 running/rescheduled clear old gates; E2 idle replaces the gate
   * set exactly; E3 error clears gates but a later recovery lifecycle may run.
   * Rules L1 running=>running/empty, L2 rescheduled=>rescheduling/empty,
   * L3 requires_action=>idle/exact ids, L4 other idle=>idle/empty,
   * L5 accepted reply->error=>error/empty, L6 error->rescheduled=>
   * rescheduling/empty while retaining last error.
   */
  it("folds every recoverable lifecycle without retaining stale gates", () => {
    const gated = event({
      id: "idle-ask",
      type: "session.status_idle",
      processed_at: "t1",
      stop_reason: { type: "requires_action", event_ids: ["u1", "u2"] },
    });
    expect([...projectManagedSessionRuntime([gated]).pendingToolIds]).toEqual(["u1", "u2"]);
    expect(projectManagedSessionRuntime([gated, event({ id: "run", type: "session.status_running", processed_at: "t2" })]))
      .toMatchObject({ phase: "running", pendingToolIds: new Set(), resolvingToolIds: new Set() });
    expect(projectManagedSessionRuntime([gated, event({ id: "retry", type: "session.status_rescheduled", processed_at: "t2" })]))
      .toMatchObject({ phase: "rescheduling", pendingToolIds: new Set(), resolvingToolIds: new Set() });
    for (const reason of ["end_turn", "retries_exhausted", "budget_reached"] as const) {
      expect(projectManagedSessionRuntime([gated, event({
        id: `idle-${reason}`,
        type: "session.status_idle",
        processed_at: "t2",
        stop_reason: { type: reason },
      })]), reason).toMatchObject({ phase: "idle", pendingToolIds: new Set(), resolvingToolIds: new Set() });
    }
    const failed = projectManagedSessionRuntime([
      gated,
      event({ id: "reply-before-error", type: "user.tool_confirmation", tool_use_id: "u1", result: "allow" }),
      event({ id: "error", type: "session.error", processed_at: "t2", error: { type: "unknown_error", message: "lost", retry_status: { type: "retrying" } } }),
    ]);
    expect(failed).toMatchObject({ phase: "error", pendingToolIds: new Set(), resolvingToolIds: new Set() });
    expect(managedSessionAdmission(failed, "idle").canSendMessage).toBe(true);
    const recovered = projectManagedSessionRuntime([
      gated,
      event({ id: "error", type: "session.error", processed_at: "t2", error: { type: "unknown_error", message: "lost", retry_status: { type: "retrying" } } }),
      event({ id: "retry", type: "session.status_rescheduled", processed_at: "t3" }),
    ]);
    expect(recovered.phase).toBe("rescheduling");
    expect(recovered.latestError?.id).toBe("error");
  });

  /**
   * Cause/effect table for each accepted reply family:
   * | Rule | family | processed_at | pending | resolving |
   * | R1 | confirmation/custom/tool_result | U | delete target | add target |
   * | R2 | confirmation/custom/tool_result | P | delete target | delete target |
   * The committed reply is the sole closure fact; null and absence are U.
   */
  it("closes or resolves every official user tool reply family", () => {
    const cases = [
      ["user.tool_confirmation", "tool_use_id"],
      ["user.custom_tool_result", "custom_tool_use_id"],
      ["user.tool_result", "tool_use_id"],
    ] as const;
    for (const [type, field] of cases) {
      const gated = event({
        id: `idle-${type}`,
        type: "session.status_idle",
        processed_at: "t0",
        stop_reason: { type: "requires_action", event_ids: ["tool"] },
      });
      const unprocessed = projectManagedSessionRuntime([
        gated,
        event({ id: `reply-${type}`, type, [field]: "tool", processed_at: null, ...(type === "user.tool_confirmation" ? { result: "allow" } : {}) }),
      ]);
      expect(unprocessed.pendingToolIds.size, type).toBe(0);
      expect([...unprocessed.resolvingToolIds], type).toEqual(["tool"]);
      expect([...unprocessed.resolvingInputIds], type).toEqual([`reply-${type}`]);
      const processed = projectManagedSessionRuntime([
        gated,
        event({ id: `reply-${type}`, type, [field]: "tool", processed_at: "t1", ...(type === "user.tool_confirmation" ? { result: "allow" } : {}) }),
      ]);
      expect(processed.pendingToolIds.size, type).toBe(0);
      expect(processed.resolvingToolIds.size, type).toBe(0);
      expect(processed.resolvingInputIds.size, type).toBe(0);
    }
  });

  /**
   * Cause/effect graph: C1 accepted inbound echo is message/confirmation/
   * custom-result/tool-result/interrupt/system-message; C2 processed_at is U/P;
   * C3 aggregate remains stale idle; C4 a later lifecycle boundary arrives.
   * Effects: E1 U records its Event id, denies another message, and permits a
   * nonterminal interrupt; E2 P records no resolving input and defers admission
   * to lifecycle+aggregate; E3 every lifecycle boundary clears prior accepted
   * input. `user.define_outcome` is constrained by the official SDK to P.
   *
   * | Rule | family | maturity | stale-idle admission |
   * | I1 | six optional-timestamp inbound families | U | deny/send, allow/stop |
   * | I2 | six optional-timestamp inbound families | P | lifecycle decides |
   * | I3 | define_outcome (required P) | P | lifecycle decides |
   */
  it("gates every accepted-but-unprocessed inbound echo by Event identity", () => {
    const idle = event({
      id: "idle-input",
      type: "session.status_idle",
      processed_at: "t0",
      stop_reason: { type: "end_turn" },
    });
    const inputs = [
      { id: "message", type: "user.message", content: [] },
      { id: "confirmation", type: "user.tool_confirmation", tool_use_id: "tool", result: "allow" },
      { id: "custom", type: "user.custom_tool_result", custom_tool_use_id: "custom", content: [] },
      { id: "tool-result", type: "user.tool_result", tool_use_id: "tool", content: [] },
      { id: "interrupt", type: "user.interrupt" },
      { id: "system", type: "system.message", content: [] },
    ];
    for (const input of inputs) {
      const unprocessed = projectManagedSessionRuntime([
        idle,
        event({ ...input, processed_at: null }),
      ]);
      expect([...unprocessed.resolvingInputIds], input.type).toEqual([input.id]);
      expect(managedSessionAdmission(unprocessed, "idle"), input.type).toMatchObject({
        canSendMessage: false,
        canInterrupt: true,
      });

      const processed = projectManagedSessionRuntime([
        idle,
        event({ ...input, processed_at: "t1" }),
      ]);
      expect(processed.resolvingInputIds.size, input.type).toBe(0);
      expect(managedSessionAdmission(processed, "idle").canSendMessage, input.type).toBe(true);
    }

    const defined = projectManagedSessionRuntime([idle, event({
      id: "outcome",
      type: "user.define_outcome",
      description: "ship",
      max_iterations: null,
      outcome_id: "outcome-1",
      processed_at: "t1",
      rubric: { type: "text", content: "works" },
    })]);
    expect(defined.resolvingInputIds.size).toBe(0);

    const accepted = event({ id: "accepted", type: "user.message", content: [] });
    const boundaries = [
      event({ id: "running-boundary", type: "session.status_running", processed_at: "t2" }),
      event({ id: "rescheduled-boundary", type: "session.status_rescheduled", processed_at: "t2" }),
      event({ id: "idle-boundary", type: "session.status_idle", processed_at: "t2", stop_reason: { type: "end_turn" } }),
      event({ id: "error-boundary", type: "session.error", processed_at: "t2", error: { type: "unknown_error", message: "lost", retry_status: { type: "exhausted" } } }),
      event({ id: "terminated-boundary", type: "session.status_terminated", processed_at: "t2" }),
      event({ id: "deleted-boundary", type: "session.deleted", processed_at: "t2" }),
    ];
    for (const boundary of boundaries) {
      expect(projectManagedSessionRuntime([idle, accepted, boundary]).resolvingInputIds.size, boundary.type)
        .toBe(0);
    }
  });

  /**
   * Cause/effect rules: C1 an agent tool or MCP result names a pending target ->
   * E1 close pending and resolving for that exact target; foreign targets have
   * no effect. This covers both official server-result families.
   */
  it("settles gates from agent tool and MCP result facts", () => {
    const gated = event({
      id: "idle",
      type: "session.status_idle",
      processed_at: "t0",
      stop_reason: { type: "requires_action", event_ids: ["tool", "mcp"] },
    });
    const settled = projectManagedSessionRuntime([
      gated,
      event({ id: "tool-result", type: "agent.tool_result", processed_at: "t1", tool_use_id: "tool", is_error: false }),
      event({ id: "mcp-result", type: "agent.mcp_tool_result", processed_at: "t2", mcp_tool_use_id: "mcp", is_error: true }),
    ]);
    expect(settled.pendingToolIds.size).toBe(0);
    expect(settled.resolvingToolIds.size).toBe(0);
  });

  /**
   * Cause/effect table: terminated/deleted is present/absent; a later stale
   * running or idle fact is present/absent. E1 terminal clears gates and is
   * absorbing; E2 deleted is the strongest terminal presentation. Rules T1
   * terminated+later running=>terminated; T2 deleted+later idle=>deleted.
   */
  it("makes terminated and deleted lifecycle facts absorbing", () => {
    const gated = event({
      id: "ask",
      type: "session.status_idle",
      processed_at: "t0",
      stop_reason: { type: "requires_action", event_ids: ["tool"] },
    });
    const running = event({ id: "running", type: "session.status_running", processed_at: "t9" });
    const idle = event({ id: "idle", type: "session.status_idle", processed_at: "t9", stop_reason: { type: "end_turn" } });
    const terminated = projectManagedSessionRuntime([
      gated,
      event({ id: "terminated", type: "session.status_terminated", processed_at: "t1" }),
      running,
    ]);
    expect(terminated.phase).toBe("terminated");
    expect(terminated.pendingToolIds.size).toBe(0);
    expect(terminated.resolvingToolIds.size).toBe(0);
    const deleted = projectManagedSessionRuntime([
      gated,
      event({ id: "deleted", type: "session.deleted", processed_at: "t1" }),
      idle,
    ]);
    expect(deleted.phase).toBe("deleted");
    expect(deleted.pendingToolIds.size).toBe(0);
    expect(deleted.resolvingToolIds.size).toBe(0);
    expect(managedSessionAdmission(terminated, "idle")).toEqual({
      canSendMessage: false,
      canResolveTools: false,
      canInterrupt: false,
    });
    expect(projectManagedSessionRuntime([
      event({ id: "terminated", type: "session.status_terminated", processed_at: "t1" }),
      event({ id: "deleted", type: "session.deleted", processed_at: "t2" }),
      running,
    ]).phase).toBe("deleted");
  });

  /**
   * Cause/effect graph: C1 aggregate is idle/running/rescheduling/terminated/
   * unknown; C2 Event phase is unknown/idle/error/working/terminal; C3 pending
   * or resolving gates exist; C4 projection is healthy. Effect E1 message input
   * is admitted only by the conservative idle join with no gate. E2 tool reply
   * is admitted only at the exact idle gate. E3 interrupt is admitted only for
   * nonterminal active/recovery work. Decision rules A1 idle+unknown/idle/error+
   * empty+healthy=>message; A2 any non-idle aggregate=>deny message; A3 any gate
   * =>deny message; A4 idle+pending=>tool reply/interrupt. Projection-unhealthy
   * recovery has its own table below, so it cannot weaken these ordinary
   * admission expectations.
   */
  it("conservatively joins aggregate and Event truth for every input consumer", () => {
    const unknown = projectManagedSessionRuntime([]);
    expect(managedSessionAdmission(unknown, "idle").canSendMessage).toBe(true);
    expect(managedSessionAdmission(unknown, "running").canSendMessage).toBe(false);
    expect(managedSessionAdmission(unknown, "rescheduling").canSendMessage).toBe(false);
    expect(managedSessionAdmission(unknown, "terminated").canSendMessage).toBe(false);
    expect(managedSessionAdmission(unknown).canSendMessage).toBe(false);

    const staleIdle = projectManagedSessionRuntime([event({
      id: "idle",
      type: "session.status_idle",
      processed_at: "t0",
      stop_reason: { type: "end_turn" },
    })]);
    expect(managedSessionAdmission(staleIdle, "running").canSendMessage).toBe(false);
    expect(managedSessionAdmission(staleIdle, "idle").canSendMessage).toBe(true);

    const pending = projectManagedSessionRuntime([event({
      id: "ask",
      type: "session.status_idle",
      processed_at: "t0",
      stop_reason: { type: "requires_action", event_ids: ["tool"] },
    })]);
    expect(managedSessionAdmission(pending, "idle")).toEqual({
      canSendMessage: false,
      canResolveTools: true,
      canInterrupt: true,
    });
    const resolving = projectManagedSessionRuntime([
      event({ id: "ask", type: "session.status_idle", processed_at: "t0", stop_reason: { type: "requires_action", event_ids: ["tool"] } }),
      event({ id: "reply", type: "user.tool_confirmation", tool_use_id: "tool", result: "allow" }),
    ]);
    expect(managedSessionAdmission(resolving, "idle").canSendMessage).toBe(false);
  });

  /**
   * Cause/effect graph: C1 the aggregate is nonterminal idle; C2 committed
   * Event projection is unhealthy, so a damaged Awaiting ticket may have no
   * visible tool card; C3 ordinary message/reply controls remain unsafe.
   * Effects: E1 deny message and reply; E2 retain one force-recovery Interrupt
   * path that uses server-side committed Run topology rather than browser
   * pending reconstruction.
   *
   * | Rule | Aggregate | Projection | Visible pending | Effects |
   * |---|---|---|---|---|
   * | D1 | idle/nonterminal | unhealthy | none/unknown | E1 + E2 |
   * | D2 | terminated | any | any | deny every control (terminal table above) |
   *
   * The aggregate is the nonterminal fence for control-only recovery; an
   * unknown aggregate or any terminal source remains fully denied.
   */
  it("keeps only force recovery available when pending projection is damaged", () => {
    const noVisiblePending = projectManagedSessionRuntime([event({
      id: "idle-before-damage",
      type: "session.status_idle",
      processed_at: "t0",
      stop_reason: { type: "end_turn" },
    })]);
    expect({
      admission: managedSessionAdmission(noVisiblePending, "idle", false),
      pendingToolCount: noVisiblePending.pendingToolIds.size,
    }).toEqual({
      admission: {
        canSendMessage: false,
        canResolveTools: false,
        canInterrupt: true,
      },
      pendingToolCount: 0,
    });
    expect(managedSessionAdmission(noVisiblePending, undefined, false)).toEqual({
      canSendMessage: false,
      canResolveTools: false,
      canInterrupt: false,
    });
    expect(managedSessionAdmission(noVisiblePending, "terminated", false)).toEqual({
      canSendMessage: false,
      canResolveTools: false,
      canInterrupt: false,
    });
  });

  /**
   * Cause/effect rules: each closed aggregate status maps to one presentation;
   * any absent/future value fails closed as unknown.
   */
  it("presents the complete official aggregate status union", () => {
    expect(managedSessionStatusPresentation("running")).toBe("running");
    expect(managedSessionStatusPresentation("idle")).toBe("idle");
    expect(managedSessionStatusPresentation("rescheduling")).toBe("rescheduling");
    expect(managedSessionStatusPresentation("terminated")).toBe("terminated");
    expect(managedSessionStatusPresentation()).toBe("unknown");
  });

  /**
   * Cause/effect table: running/rescheduling means active work; idle/terminated
   * means inactive; absence means no selected row; a supplied future/malformed
   * value fails closed as active. All aggregate-only consumers reuse this rule
   * so polling, filters, and eligibility cannot drift.
   */
  it("classifies the complete official active status union", () => {
    expect(isManagedSessionActiveStatus("running")).toBe(true);
    expect(isManagedSessionActiveStatus("rescheduling")).toBe(true);
    expect(isManagedSessionActiveStatus("idle")).toBe(false);
    expect(isManagedSessionActiveStatus("terminated")).toBe(false);
    expect(isManagedSessionActiveStatus()).toBe(false);
    expect(isManagedSessionActiveStatus("future-status")).toBe(true);
  });

  /**
   * Cause/effect table: C1 aggregate is active/idle/terminal/unknown; C2 Event
   * phase is working/idle/error/terminal. E1 terminal or active evidence cannot
   * be hidden by stale idle; E2 idle/error cannot override aggregate active or
   * unknown. Rules P1 terminal-any=>terminal, P2 working-any=>working, P3
   * aggregate-idle+error=>error, P4 unknown aggregate+idle=>unknown.
   */
  it("presents aggregate and Event state without contradictory idle", () => {
    const idle = projectManagedSessionRuntime([event({
      id: "idle",
      type: "session.status_idle",
      processed_at: "t0",
      stop_reason: { type: "end_turn" },
    })]);
    const running = projectManagedSessionRuntime([event({ id: "run", type: "session.status_running", processed_at: "t1" })]);
    const failed = projectManagedSessionRuntime([event({
      id: "error",
      type: "session.error",
      processed_at: "t1",
      error: { type: "unknown_error", message: "lost", retry_status: { type: "exhausted" } },
    })]);
    expect(managedSessionPresentationPhase(idle, "running")).toBe("running");
    expect(managedSessionPresentationPhase(running, "idle")).toBe("running");
    expect(managedSessionPresentationPhase(failed, "idle")).toBe("error");
    expect(managedSessionPresentationPhase(idle)).toBe("unknown");
    expect(managedSessionPresentationPhase(running, "terminated")).toBe("terminated");
  });
});

describe("Managed Session volatile preview decision table", () => {
  /**
   * Cause/effect graph: C1 message/thinking start owns id M/T; C2 delta id is
   * matching/foreign; C3 committed id is matching/foreign. Effects: E1 a new
   * start replaces the active preview; E2 only matching message delta appends;
   * E3 matching commit clears; E4 thinking retains its id and accepts no text.
   */
  it("tracks message and thinking previews by exact event identity", () => {
    const message = reduceLivePreview(EMPTY_LIVE_PREVIEW, stream({
      type: "event_start",
      event: { id: "message", type: "agent.message" },
    }));
    const foreignDelta = reduceLivePreview(message, stream({
      type: "event_delta",
      event_id: "other",
      delta: { type: "content_delta", content: { type: "text", text: "ignore" } },
    }));
    const appended = reduceLivePreview(foreignDelta, stream({
      type: "event_delta",
      event_id: "message",
      delta: { type: "content_delta", content: { type: "text", text: "hello" } },
    }));
    expect(appended).toEqual({ eventId: "message", text: "hello", thinking: false });
    expect(reduceLivePreview(appended, stream({ id: "other", type: "agent.message", content: [], processed_at: "t" })))
      .toBe(appended);
    expect(reduceLivePreview(appended, stream({ id: "message", type: "agent.message", content: [], processed_at: "t" })))
      .toEqual(EMPTY_LIVE_PREVIEW);

    const thinking = reduceLivePreview(message, stream({
      type: "event_start",
      event: { id: "thinking", type: "agent.thinking" },
    }));
    expect(thinking).toEqual({ eventId: "thinking", text: "", thinking: true });
    expect(reduceLivePreview(thinking, stream({
      type: "event_delta",
      event_id: "other",
      delta: { type: "content_delta", content: { type: "text", text: "hidden" } },
    }))).toBe(thinking);
    expect(reduceLivePreview(thinking, stream({
      type: "event_delta",
      event_id: "thinking",
      delta: { type: "content_delta", content: { type: "text", text: "hidden" } },
    }))).toBe(thinking);
    expect(reduceLivePreview(thinking, stream({ id: "other", type: "agent.thinking", processed_at: "t" })))
      .toBe(thinking);
    expect(reduceLivePreview(thinking, stream({ id: "thinking", type: "agent.thinking", processed_at: "t" })))
      .toEqual(EMPTY_LIVE_PREVIEW);
    expect(reduceLivePreview(thinking, stream({
      type: "event_start",
      event: { id: "future", type: "agent.future" },
    }))).toEqual(EMPTY_LIVE_PREVIEW);
  });

  /**
   * Cause/effect table: C1 active preview exists; C2 abort is model-request-end,
   * idle, error, terminated, or deleted. Each rule yields E1 empty preview even
   * when no matching buffered Event was produced. This is the SDK's early-end
   * contract; ordinary foreign committed facts do not abort.
   */
  it("clears every complete or aborted preview boundary", () => {
    const active = { eventId: "message", text: "partial", thinking: false };
    const aborts = [
      { id: "model-end", type: "span.model_request_end" },
      { id: "idle", type: "session.status_idle", stop_reason: { type: "end_turn" } },
      { id: "error", type: "session.error", error: { type: "unknown_error", message: "lost", retry_status: { type: "exhausted" } } },
      { id: "terminated", type: "session.status_terminated" },
      { id: "deleted", type: "session.deleted" },
    ];
    for (const abort of aborts) {
      expect(reduceLivePreview(active, stream({ ...abort, processed_at: "t" })), abort.type)
        .toEqual(EMPTY_LIVE_PREVIEW);
    }
  });
});
