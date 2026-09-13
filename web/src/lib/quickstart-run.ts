// The Quickstart first run's creation request. Publication and session creation
// stay in the editor surface; only the exact request shape lives here, so the
// rules that bind a Session to a reviewed publication — and the ordering the
// Outcome grader depends on — are provable without driving the editor.

import { BUILTIN_LOCAL_ENVIRONMENT_ID } from "./api/client";
import type { CreateSessionRequest } from "./api/types";
import { defineOutcomeEvent } from "./session-outcome";
import type { OutcomeDraft } from "./session-outcome";

export interface QuickRunSessionInput {
  agentId: string;
  /** The revision the publish boundary acknowledged, never the draft's own. */
  sourceRevision: number;
  environmentId?: string;
  vaultIds?: string[];
  task: string;
  outcome?: OutcomeDraft;
  title: string;
}

export function quickRunSessionRequest(input: QuickRunSessionInput): CreateSessionRequest {
  return {
    // Publish is the immutable execution boundary. Pin the Session to the exact
    // revision acknowledged by that boundary so an active-active Coordinator can
    // either load that publication or fail creation closed; it must never run an
    // unversioned fallback configuration.
    agent: { id: input.agentId, type: "agent", version: input.sourceRevision },
    environment_id: input.environmentId ?? BUILTIN_LOCAL_ENVIRONMENT_ID,
    ...(input.vaultIds?.length ? { vault_ids: input.vaultIds } : {}),
    // The official create boundary owns initial_events atomically. A second POST
    // would create a partial Session on transport failure and duplicate the
    // idempotency/retry semantics creation already owns. At most one
    // `user.define_outcome` is admitted, and the grader needs the task it is
    // grading, so the Outcome follows the first message in the same batch.
    initial_events: [
      { type: "user.message", content: [{ type: "text", text: input.task }] },
      ...(input.outcome ? [defineOutcomeEvent(input.outcome)] : []),
    ],
    title: input.title,
  };
}
