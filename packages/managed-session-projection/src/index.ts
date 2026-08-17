import type {
  BetaManagedAgentsSessionEvent,
  BetaManagedAgentsStreamSessionEvents,
} from "@anthropic-ai/sdk/resources/beta/sessions/events";

export type ManagedEvent = BetaManagedAgentsSessionEvent;
export type ManagedStreamEvent = BetaManagedAgentsStreamSessionEvents;

type JsonObject = Record<string, unknown>;

function object(value: unknown): JsonObject | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? value as JsonObject
    : undefined;
}

export function mergeCommittedEvents<T extends { readonly id: string }>(
  current: readonly T[],
  incoming: readonly T[],
): T[] {
  const byId = new Map(current.map((event) => [event.id, event]));
  for (const event of incoming) byId.set(event.id, event);
  return [...byId.values()];
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

export function reduceLivePreview(
  current: LivePreview,
  event: ManagedStreamEvent,
): LivePreview {
  const value = event as unknown as JsonObject;
  if (event.type === "event_start") {
    const started = object(value.event);
    if (started?.type === "agent.message" && typeof started.id === "string") {
      return { eventId: started.id, text: "", thinking: false };
    }
    if (started?.type === "agent.thinking") {
      return { text: "", thinking: true };
    }
  }
  if (event.type === "event_delta" && value.event_id === current.eventId) {
    const delta = object(value.delta);
    const content = object(delta?.content);
    if (content?.type === "text" && typeof content.text === "string") {
      return { ...current, text: current.text + content.text };
    }
  }
  if (
    value.id === current.eventId
    || ["session.status_idle", "session.status_terminated", "session.error"].includes(event.type)
  ) {
    return EMPTY_LIVE_PREVIEW;
  }
  return current;
}

export function isCommittedStreamEvent(
  event: ManagedStreamEvent,
): event is ManagedEvent {
  return event.type !== "event_start"
    && event.type !== "event_delta"
    && typeof (event as unknown as JsonObject).id === "string";
}
