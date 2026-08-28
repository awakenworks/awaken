import type {
  BetaManagedAgentsSessionEvent,
} from "@anthropic-ai/sdk-current/resources/beta/sessions/events";

export type {
  BetaManagedAgentsEventParams,
  BetaManagedAgentsSendSessionEvents,
  BetaManagedAgentsSessionEvent,
  BetaManagedAgentsSessionEventsPageCursor,
  BetaManagedAgentsStreamSessionEvents,
} from "@anthropic-ai/sdk-current/resources/beta/sessions/events";

export type {
  BetaManagedAgentsSession,
  BetaManagedAgentsSessionsBidirectionalPageCursor,
  BetaManagedAgentsSessionUsage,
  SessionCreateParams,
  SessionUpdateParams,
} from "@anthropic-ai/sdk-current/resources/beta/sessions/sessions";

export type {
  BetaManagedAgentsSessionResource,
  BetaManagedAgentsSessionResourcesPageCursor,
} from "@anthropic-ai/sdk-current/resources/beta/sessions/resources";

export type {
  BetaManagedAgentsSessionThread,
  BetaManagedAgentsSessionThreadsPageCursor,
  BetaManagedAgentsSessionThreadUsage,
} from "@anthropic-ai/sdk-current/resources/beta/sessions/threads/threads";

type EventContentBlock<Event> = Event extends {
  content?: Array<infer Block>;
} ? Block : never;

/** Every top-level content block carried by the current committed Event union. */
export type ManagedSessionContentBlock = EventContentBlock<BetaManagedAgentsSessionEvent>;
