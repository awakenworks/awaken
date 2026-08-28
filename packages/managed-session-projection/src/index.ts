import type {
  BetaManagedAgentsSession,
  BetaManagedAgentsSessionEvent,
  BetaManagedAgentsStreamSessionEvents,
} from "@awaken/managed-sdk-oracle/current-types";
import {
  MANAGED_SESSION_EVENT_TYPES,
  MANAGED_SESSION_PREVIEW_TYPES,
} from "@awaken/managed-sdk-oracle/current-wire";

export {
  MANAGED_SESSION_EVENT_TYPES,
  MANAGED_SESSION_PREVIEW_TYPES,
} from "@awaken/managed-sdk-oracle/current-wire";

export type ManagedEvent = BetaManagedAgentsSessionEvent;
export type ManagedStreamEvent = BetaManagedAgentsStreamSessionEvents;
export type ManagedSessionStatus = BetaManagedAgentsSession["status"];

type JsonObject = Record<string, unknown>;

function object(value: unknown): JsonObject | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? value as JsonObject
    : undefined;
}

function jsonEquivalent(left: unknown, right: unknown): boolean {
  if (left === right) return true;
  if (Array.isArray(left) || Array.isArray(right)) {
    if (!Array.isArray(left) || !Array.isArray(right) || left.length !== right.length) return false;
    return left.every((value, index) => {
      const leftPresent = value !== undefined;
      const rightPresent = right[index] !== undefined;
      return leftPresent === rightPresent
        && (!leftPresent || jsonEquivalent(value, right[index]));
    });
  }
  const leftObject = object(left);
  const rightObject = object(right);
  if (!leftObject || !rightObject) return false;
  const leftKeys = Object.keys(leftObject).filter((key) => leftObject[key] !== undefined).sort();
  const rightKeys = Object.keys(rightObject).filter((key) => rightObject[key] !== undefined).sort();
  return leftKeys.length === rightKeys.length
    && leftKeys.every((key, index) => key === rightKeys[index]
      && jsonEquivalent(leftObject[key], rightObject[key]));
}

function immutableEventMaterial(event: { readonly id: string }): JsonObject {
  const { processed_at: _processedAt, ...immutable } = event as JsonObject & { processed_at?: unknown };
  return immutable;
}

function processedAt(event: { readonly id: string }): string | undefined {
  const value = (event as JsonObject).processed_at;
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

export class ManagedProjectionConflictError extends Error {
  readonly eventId: string;

  constructor(eventId: string, reason: string) {
    super(`committed event ${eventId} conflicts with its immutable projection: ${reason}`);
    this.name = "ManagedProjectionConflictError";
    this.eventId = eventId;
  }
}

function reconcileCommittedEvent<T extends { readonly id: string }>(current: T, incoming: T): T {
  if (!jsonEquivalent(immutableEventMaterial(current), immutableEventMaterial(incoming))) {
    throw new ManagedProjectionConflictError(current.id, "payload changed");
  }
  const currentProcessedAt = processedAt(current);
  const incomingProcessedAt = processedAt(incoming);
  if (currentProcessedAt && incomingProcessedAt && currentProcessedAt !== incomingProcessedAt) {
    throw new ManagedProjectionConflictError(current.id, "processed_at changed after commit");
  }
  if (!currentProcessedAt && incomingProcessedAt) {
    return { ...current, processed_at: incomingProcessedAt };
  }
  return current;
}

/**
 * Reconcile committed history/SSE overlap by immutable Event identity. The only
 * legal enrichment is `processed_at: absent|null -> non-empty string`; delayed
 * frames can never erase it or rewrite committed material.
 */
export function mergeCommittedEvents<T extends { readonly id: string }>(
  current: readonly T[],
  incoming: readonly T[],
): T[] {
  const result = [...current];
  const byId = new Map<string, { index: number; event: T }>();
  for (const [index, event] of current.entries()) {
    if (byId.has(event.id)) {
      throw new ManagedProjectionConflictError(event.id, "duplicate id in current history");
    }
    byId.set(event.id, { index, event });
  }
  let changed = false;
  for (const event of incoming) {
    const existing = byId.get(event.id);
    if (!existing) {
      const index = result.length;
      result.push(event);
      byId.set(event.id, { index, event });
      changed = true;
      continue;
    }
    const reconciled = reconcileCommittedEvent(existing.event, event);
    if (reconciled !== existing.event) {
      result[existing.index] = reconciled;
      byId.set(event.id, { ...existing, event: reconciled });
      changed = true;
    }
  }
  return changed ? result : current as T[];
}

/** Count new identities and legal `processed_at` enrichments without a second merge policy. */
export function countCommittedEventUpdates<T extends { readonly id: string }>(
  current: readonly T[],
  incoming: readonly T[],
): number {
  let projected = current;
  let count = 0;
  for (const event of incoming) {
    const next = mergeCommittedEvents(projected, [event]);
    if (next !== projected) count += 1;
    projected = next;
  }
  return count;
}

export type ManagedSessionRuntimePhase =
  | "unknown"
  | "running"
  | "rescheduling"
  | "idle"
  | "error"
  | "terminated"
  | "deleted";

export interface ManagedSessionRuntimeProjection {
  phase: ManagedSessionRuntimePhase;
  pendingToolIds: Set<string>;
  resolvingToolIds: Set<string>;
  /** Accepted inbound Event ids that lack their processed commit anchor. */
  resolvingInputIds: Set<string>;
  latestError?: ManagedEvent;
}

function closeTool(
  event: ManagedEvent,
  id: string,
  pending: Set<string>,
  resolving: Set<string>,
): void {
  pending.delete(id);
  if (processedAt(event)) resolving.delete(id);
  else resolving.add(id);
}

function updateResolvingInput(event: ManagedEvent, resolving: Set<string>): void {
  if (!event.type.startsWith("user.") && event.type !== "system.message") return;
  if (processedAt(event)) resolving.delete(event.id);
  else resolving.add(event.id);
}

/** Fold official committed events into the single browser Runtime projection. */
export function projectManagedSessionRuntime(
  log: readonly ManagedEvent[],
): ManagedSessionRuntimeProjection {
  let phase: ManagedSessionRuntimePhase = "unknown";
  let pending = new Set<string>();
  let resolving = new Set<string>();
  let resolvingInputs = new Set<string>();
  let latestError: ManagedEvent | undefined;
  for (const event of log) {
    if (phase === "deleted") continue;
    if (phase === "terminated") {
      // Deletion is the only stronger terminal fact; stale work cannot revive
      // a terminated Session, but a later durable delete must remain visible.
      if (event.type === "session.deleted") phase = "deleted";
      continue;
    }
    updateResolvingInput(event, resolvingInputs);
    switch (event.type) {
      case "session.status_running":
        phase = "running";
        pending = new Set();
        resolving = new Set();
        resolvingInputs = new Set();
        break;
      case "session.status_rescheduled":
        phase = "rescheduling";
        pending = new Set();
        resolving = new Set();
        resolvingInputs = new Set();
        break;
      case "session.status_idle":
        phase = "idle";
        pending = new Set(event.stop_reason.type === "requires_action"
          ? event.stop_reason.event_ids
          : []);
        resolving = new Set();
        resolvingInputs = new Set();
        break;
      case "session.error":
        phase = "error";
        pending = new Set();
        resolving = new Set();
        resolvingInputs = new Set();
        latestError = event;
        break;
      case "session.status_terminated":
        phase = "terminated";
        pending = new Set();
        resolving = new Set();
        resolvingInputs = new Set();
        break;
      case "session.deleted":
        phase = "deleted";
        pending = new Set();
        resolving = new Set();
        resolvingInputs = new Set();
        break;
      case "user.tool_confirmation":
      case "user.tool_result":
        closeTool(event, event.tool_use_id, pending, resolving);
        break;
      case "user.custom_tool_result":
        closeTool(event, event.custom_tool_use_id, pending, resolving);
        break;
      case "agent.tool_result":
        pending.delete(event.tool_use_id);
        resolving.delete(event.tool_use_id);
        break;
      case "agent.mcp_tool_result":
        pending.delete(event.mcp_tool_use_id);
        resolving.delete(event.mcp_tool_use_id);
        break;
      default:
        break;
    }
  }
  return {
    phase,
    pendingToolIds: pending,
    resolvingToolIds: resolving,
    resolvingInputIds: resolvingInputs,
    ...(latestError ? { latestError } : {}),
  };
}

export type ManagedSessionStatusPresentation =
  | "running"
  | "idle"
  | "rescheduling"
  | "terminated"
  | "unknown";

/** Closed aggregate predicate shared by every active-work consumer. */
export function isManagedSessionActiveStatus(
  status?: string,
): boolean {
  switch (status) {
    case "running":
    case "rescheduling":
      return true;
    case "idle":
    case "terminated":
      return false;
    case undefined:
      return false;
    default:
      // A supplied but unrecognized status is unavailable evidence. Treat it
      // as active so filters, polling, and eligibility all fail closed.
      return true;
  }
}

/** Closed presentation of the official aggregate status. */
export function managedSessionStatusPresentation(
  status?: ManagedSessionStatus,
): ManagedSessionStatusPresentation {
  switch (status) {
    case "running":
      return "running";
    case "idle":
      return "idle";
    case "rescheduling":
      return "rescheduling";
    case "terminated":
      return "terminated";
    default:
      return "unknown";
  }
}

/**
 * Conservative status presentation for a detail view that has both sources.
 * Terminal facts win; active work from either source wins over a stale idle
 * peer; an Event idle/error can never impersonate a known aggregate active or
 * unavailable state.
 */
export function managedSessionPresentationPhase(
  runtime: ManagedSessionRuntimeProjection,
  aggregateStatus?: ManagedSessionStatus,
): ManagedSessionRuntimePhase {
  if (runtime.phase === "deleted" || runtime.phase === "terminated") return runtime.phase;
  if (aggregateStatus === "terminated") return "terminated";
  if (runtime.phase === "running" || runtime.phase === "rescheduling") return runtime.phase;
  if (aggregateStatus === "running" || aggregateStatus === "rescheduling") return aggregateStatus;
  if (aggregateStatus === "idle") return runtime.phase === "error" ? "error" : "idle";
  return "unknown";
}

export interface ManagedSessionAdmission {
  canSendMessage: boolean;
  canResolveTools: boolean;
  canInterrupt: boolean;
}

/**
 * Conservatively join aggregate status with committed Event truth. An idle Event
 * cannot overrule a running, rescheduling, terminated, or unavailable aggregate.
 */
export function managedSessionAdmission(
  runtime: ManagedSessionRuntimeProjection,
  aggregateStatus?: ManagedSessionStatus,
  projectionHealthy = true,
): ManagedSessionAdmission {
  const terminal = aggregateStatus === "terminated"
    || runtime.phase === "terminated"
    || runtime.phase === "deleted";
  const hasPending = runtime.pendingToolIds.size > 0;
  const hasResolving = runtime.resolvingToolIds.size > 0
    || runtime.resolvingInputIds.size > 0;
  const eventAllowsMessage = runtime.phase === "unknown"
    || runtime.phase === "idle"
    || runtime.phase === "error";
  return {
    canSendMessage: projectionHealthy
      && aggregateStatus === "idle"
      && eventAllowsMessage
      && !hasPending
      && !hasResolving,
    canResolveTools: projectionHealthy
      && aggregateStatus === "idle"
      && runtime.phase === "idle"
      && hasPending,
    canInterrupt: projectionHealthy
      && !terminal
      && (aggregateStatus === "running"
        || aggregateStatus === "rescheduling"
        || runtime.phase === "running"
        || runtime.phase === "rescheduling"
        || hasPending
        || hasResolving),
  };
}

export interface LivePreview {
  eventId?: string;
  text: string;
  thinking: boolean;
}

export const EMPTY_LIVE_PREVIEW: LivePreview = {
  text: "",
  thinking: false,
};

const previewTypes: ReadonlySet<string> = new Set(MANAGED_SESSION_PREVIEW_TYPES);

export function isCommittedStreamEvent(
  event: ManagedStreamEvent,
): event is ManagedEvent {
  return !previewTypes.has(event.type)
    && typeof (event as unknown as JsonObject).id === "string";
}

function abortsLivePreview(type: ManagedStreamEvent["type"]): boolean {
  switch (type) {
    case "span.model_request_end":
    case "session.status_idle":
    case "session.error":
    case "session.status_terminated":
    case "session.deleted":
      return true;
    default:
      return false;
  }
}

export function reduceLivePreview(
  current: LivePreview,
  event: ManagedStreamEvent,
): LivePreview {
  const value = event as unknown as JsonObject;
  if (event.type === "event_start") {
    const started = object(value.event);
    if (typeof started?.id === "string" && started.type === "agent.message") {
      return { eventId: started.id, text: "", thinking: false };
    }
    if (typeof started?.id === "string" && started.type === "agent.thinking") {
      return { eventId: started.id, text: "", thinking: true };
    }
    // A future/malformed start still supersedes the old volatile preview. The
    // unknown new preview is ignored until its canonical committed Event.
    return EMPTY_LIVE_PREVIEW;
  }
  if (
    event.type === "event_delta"
    && !current.thinking
    && value.event_id === current.eventId
  ) {
    const delta = object(value.delta);
    const content = object(delta?.content);
    if (content?.type === "text" && typeof content.text === "string") {
      return { ...current, text: current.text + content.text };
    }
  }
  if (
    (isCommittedStreamEvent(event) && value.id === current.eventId)
    || abortsLivePreview(event.type)
  ) {
    return EMPTY_LIVE_PREVIEW;
  }
  return current;
}

// Compile-time catalog closure: every generated committed type remains part of
// the official event union consumed above. Runtime values are exported unchanged.
const _committedCatalog: ReadonlySet<ManagedEvent["type"]> = new Set(MANAGED_SESSION_EVENT_TYPES);
void _committedCatalog;
