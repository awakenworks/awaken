// The live session-log hook: an events query + committed-replay SSE folded into
// a pending buffer, plus a `send` mutation for inbound events. Live frames land
// in `pending` (surfaced as an "N new · Refresh" affordance) instead of tearing
// the reading position. Shared by every transcript projection.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useMemo, useState } from "react";
import { api, streamUrl } from "./api/client";
import type { InboundEvent, ListEventsResponse, SessionEvent } from "./api/types";
import {
  SSE_EVENT_NAMES,
  isRunning,
  mergeEvents,
  pairToolResults,
  pendingConfirmIds,
} from "./session-log";

export interface SessionLog {
  log: SessionEvent[];
  results: Map<string, SessionEvent>;
  pendingIds: Set<string>;
  running: boolean;
  freshCount: number;
  applyPending: () => void;
  send: (events: InboundEvent[]) => void;
  sendError: Error | null;
  loadError: Error | null;
  refetch: () => void;
}

/**
 * @param base    the session base path (already workspace-scoped via ws()).
 * @param queryKey a stable cache key for this session's events.
 * @param live    subscribe to the SSE stream + poll (default true).
 */
export function useSessionLog(
  base: string,
  queryKey: readonly unknown[],
  live = true,
): SessionLog {
  const qc = useQueryClient();
  const [pending, setPending] = useState<SessionEvent[]>([]);

  const events = useQuery({
    queryKey,
    queryFn: async () => (await api.get<ListEventsResponse>(`${base}/events`)).data,
    refetchInterval: live ? 5_000 : false,
  });
  const log = useMemo(() => events.data ?? [], [events.data]);

  // Committed-replay SSE → pending buffer → banner (never tears the position).
  useEffect(() => {
    if (!live) return;
    const source = new EventSource(streamUrl(`${base}/events/stream`));
    const onAny = (raw: MessageEvent) => {
      try {
        const ev = JSON.parse(raw.data as string) as SessionEvent;
        if (!ev.id) return;
        setPending((buf) => (buf.some((b) => b.id === ev.id) ? buf : [...buf, ev]));
      } catch {
        /* non-JSON frame */
      }
    };
    for (const name of SSE_EVENT_NAMES) source.addEventListener(name, onAny);
    source.onerror = () => source.close();
    return () => source.close();
  }, [base, live]);

  const freshCount = pending.filter((p) => !log.some((e) => e.id === p.id)).length;
  const applyPending = () => {
    qc.setQueryData<SessionEvent[]>(queryKey, (old) => mergeEvents(old ?? [], pending));
    setPending([]);
  };

  const send = useMutation({
    mutationFn: (evs: InboundEvent[]) => api.post(`${base}/events`, { events: evs }),
    onSuccess: () => void events.refetch(),
  });

  return {
    log,
    results: pairToolResults(log),
    pendingIds: pendingConfirmIds(log),
    running: isRunning(log),
    freshCount,
    applyPending,
    send: (evs) => send.mutate(evs),
    sendError: send.error instanceof Error ? send.error : null,
    loadError: events.error instanceof Error ? events.error : null,
    refetch: () => void events.refetch(),
  };
}
