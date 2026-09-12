// Outcome authoring. The read side already exists: the Managed Session resource
// carries `outcome_evaluations`, and the Transcript renders the evaluation
// spans. This module owns only the write — the one place operator input becomes
// a `user.define_outcome` event — so both entry points (a live Session and the
// Quickstart first run) construct the exact same wire shape.

import type { InboundEvent, Session } from "./api/types";

/** The exact member of the inbound union this module produces. Session
 * creation accepts only `user.message` and `user.define_outcome`, so the
 * narrow type is what lets one builder serve both `initial_events` and the
 * events endpoint without a cast at either call site. */
export type DefineOutcomeEvent = Extract<InboundEvent, { type: "user.define_outcome" }>;

/** Managed Agents grades an Outcome over eval→revision cycles. The wire default
 * is 3 and the documented ceiling is 20. Session creation validates the range;
 * `POST .../events` does not re-validate it, so the console owns the clamp for
 * both paths rather than trusting one boundary to cover the other. */
export const OUTCOME_ITERATIONS_MIN = 1;
export const OUTCOME_ITERATIONS_MAX = 20;
export const OUTCOME_ITERATIONS_DEFAULT = 3;

/** The text rubric ceiling stated by the wire contract. */
export const OUTCOME_RUBRIC_MAX_CHARS = 262_144;

/** Operator-facing draft. `rubric` stays plain text here; the file-sourced
 * rubric variant is a separate capability and is not authored in this form. */
export interface OutcomeDraft {
  description: string;
  rubric: string;
  maxIterations: number;
}

export function emptyOutcomeDraft(): OutcomeDraft {
  return { description: "", rubric: "", maxIterations: OUTCOME_ITERATIONS_DEFAULT };
}

export function clampOutcomeIterations(value: number): number {
  if (!Number.isFinite(value)) return OUTCOME_ITERATIONS_DEFAULT;
  return Math.min(
    OUTCOME_ITERATIONS_MAX,
    Math.max(OUTCOME_ITERATIONS_MIN, Math.trunc(value)),
  );
}

/** Both fields are rejected blank by the create boundary, and the events
 * endpoint does not re-check them. Refuse to construct an event the runtime
 * would reject or grade against nothing. */
export function outcomeDraftComplete(draft: OutcomeDraft): boolean {
  const rubric = draft.rubric.trim();
  return draft.description.trim().length > 0
    && rubric.length > 0
    && rubric.length <= OUTCOME_RUBRIC_MAX_CHARS;
}

/** The exact inbound event. Callers must gate on `outcomeDraftComplete` first;
 * this only normalizes what a complete draft already proved. */
export function defineOutcomeEvent(draft: OutcomeDraft): DefineOutcomeEvent {
  return {
    type: "user.define_outcome",
    description: draft.description.trim(),
    rubric: { type: "text", content: draft.rubric.trim() },
    max_iterations: clampOutcomeIterations(draft.maxIterations),
  };
}

/** The Outcome the runtime has not finished grading, if any. The aggregate
 * drives one Outcome at a time and back-pressures a second definition into a
 * pending batch instead of failing it, so the console disables authoring rather
 * than accepting an event that would wait invisibly. */
export function activeOutcome(session: Session | undefined) {
  return (session?.outcome_evaluations ?? []).find(
    (evaluation) => evaluation.completed_at == null,
  );
}
