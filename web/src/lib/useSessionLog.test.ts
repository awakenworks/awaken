import { QueryClient } from "@tanstack/react-query";
import { describe, expect, it, vi } from "vitest";
import {
  ManagedProjectionConflictError,
  managedSessionAdmission,
  projectManagedSessionRuntime,
} from "@awaken/managed-session-projection";
import type { SessionEvent } from "./api/types";
import {
  gateManagedSessionAdmissionWhileSending,
  mergeCommittedSessionCache,
  reconcileSessionSendResponse,
  sessionProjectionErrorKey,
} from "./useSessionLog";

const event = (value: Record<string, unknown>): SessionEvent => value as unknown as SessionEvent;
const idle = event({
  id: "idle",
  type: "session.status_idle",
  processed_at: "t0",
  stop_reason: { type: "end_turn" },
});

describe("Session receipt-to-query projection decision table", () => {
  /**
   * Cause/effect rules: C1 POST receipt is user.message/user.interrupt; C2 its
   * processed_at is U/P while aggregate is stale idle. Effects: E1 the shared
   * query changes synchronously; E2 U immediately records resolving input,
   * denies another message, and permits interrupt; E3 the same immutable receipt
   * enriched to P keeps its position and lets lifecycle+aggregate decide.
   */
  it("projects accepted message and interrupt receipts before refetch", () => {
    const receipts = [
      { id: "message", type: "user.message", content: [] },
      { id: "interrupt", type: "user.interrupt" },
    ];
    for (const receipt of receipts) {
      const qc = new QueryClient();
      const key = ["session-events", receipt.id] as const;
      qc.setQueryData<SessionEvent[]>(key, [idle]);

      mergeCommittedSessionCache(qc, key, [event({ ...receipt, processed_at: null })]);
      const unprocessed = projectManagedSessionRuntime(qc.getQueryData<SessionEvent[]>(key) ?? []);
      expect([...unprocessed.resolvingInputIds], receipt.type).toEqual([receipt.id]);
      expect(managedSessionAdmission(unprocessed, "idle"), receipt.type).toMatchObject({
        canSendMessage: false,
        canInterrupt: true,
      });

      mergeCommittedSessionCache(qc, key, [event({ ...receipt, processed_at: "t1" })]);
      const committed = qc.getQueryData<SessionEvent[]>(key) ?? [];
      const processed = projectManagedSessionRuntime(committed);
      expect(committed.map(({ id }) => id), receipt.type).toEqual(["idle", receipt.id]);
      expect(processed.resolvingInputIds.size, receipt.type).toBe(0);
      expect(managedSessionAdmission(processed, "idle").canSendMessage, receipt.type).toBe(true);
    }
  });

  /**
   * Cause/effect rule: C1 a POST/SSE receipt reuses a committed id but changes
   * immutable material -> E1 keep the first fact, E2 publish the typed projection
   * conflict on the shared health key, and E3 deny admission even if stale
   * aggregate/event lifecycle would otherwise allow input.
   */
  it("publishes a conflicting receipt and fails admission closed", () => {
    const qc = new QueryClient();
    const key = ["session-events", "conflict"] as const;
    const first = event({
      id: "message",
      type: "user.message",
      content: [{ type: "text", text: "first" }],
      processed_at: "t1",
    });
    qc.setQueryData<SessionEvent[]>(key, [idle, first]);

    expect(() => mergeCommittedSessionCache(qc, key, [event({
      ...first,
      content: [{ type: "text", text: "changed" }],
    })])).toThrow(ManagedProjectionConflictError);

    expect(qc.getQueryData<SessionEvent[]>(key)).toEqual([idle, first]);
    const projectionError = qc.getQueryData<Error>(sessionProjectionErrorKey(key));
    expect(projectionError).toBeInstanceOf(ManagedProjectionConflictError);
    const runtime = projectManagedSessionRuntime(qc.getQueryData<SessionEvent[]>(key) ?? []);
    expect(managedSessionAdmission(runtime, "idle", projectionError == null).canSendMessage)
      .toBe(false);
  });

  /**
   * Cause/effect rule: C1 the official send response omits optional `data`;
   * C2 the cached aggregate/events can still be stale idle. Effect: the shared
   * mutation remains pending until its authoritative events refetch settles,
   * so no transport-complete window can reopen admission from stale state.
   */
  it("waits for authoritative reconciliation when a send response omits data", async () => {
    const qc = new QueryClient();
    const key = ["session-events", "optional-receipt"] as const;
    let resolveRefetch!: () => void;
    const refetch = vi.fn(() => new Promise<void>((resolve) => {
      resolveRefetch = resolve;
    }));
    let settled = false;

    const reconciliation = reconcileSessionSendResponse(qc, key, {}, refetch)
      .then(() => { settled = true; });
    await Promise.resolve();

    expect(refetch).toHaveBeenCalledOnce();
    expect(settled).toBe(false);

    resolveRefetch();
    await reconciliation;
    expect(settled).toBe(true);
  });

  /**
   * Cause/effect rule: C1 an SSE observation for t2 arrives before the in-flight
   * official ascending history [t1,t2]; C2 t2 overlaps by immutable identity.
   * Effects: E1 reconcile through the same identity/maturity merge, E2 rebuild
   * distinct-id order from authoritative history, and E3 project the actual
   * latest idle state instead of the arrival-order running inversion.
   */
  it("restores authoritative history order after an SSE-before-list race", () => {
    const qc = new QueryClient();
    const key = ["session-events", "ordering-race"] as const;
    const running = event({
      id: "running",
      type: "session.status_running",
      processed_at: "t1",
    });

    mergeCommittedSessionCache(qc, key, [idle]);
    mergeCommittedSessionCache(
      qc,
      key,
      [running, event({ ...idle })],
      "authoritative-history",
    );

    const ordered = qc.getQueryData<SessionEvent[]>(key) ?? [];
    expect(ordered.map(({ id }) => id)).toEqual(["running", "idle"]);
    expect(ordered[1]).toBe(idle);
    const runtime = projectManagedSessionRuntime(ordered);
    expect(runtime.phase).toBe("idle");
    expect(managedSessionAdmission(runtime, "idle").canSendMessage).toBe(true);
  });

  /**
   * Cause/effect table: C1 one shared detail input transport is pending/not
   * pending. E1 pending denies message, approval, and interrupt together; E2
   * settled transport exposes the unchanged committed-projection admission.
   * This prevents two controls from creating concurrent transport intents.
   */
  it("gates every input control with the one shared pending transport", () => {
    const projected = {
      canSendMessage: true,
      canResolveTools: true,
      canInterrupt: true,
    };
    expect(gateManagedSessionAdmissionWhileSending(projected, true)).toEqual({
      canSendMessage: false,
      canResolveTools: false,
      canInterrupt: false,
    });
    expect(gateManagedSessionAdmissionWhileSending(projected, false)).toBe(projected);
  });
});
