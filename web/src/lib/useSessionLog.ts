// The live Session-log application hook: one committed history projection,
// generated-catalog SSE subscription, and official input mutation seam.

import { useMutation, useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import { useEffect, useMemo, useRef, useState } from "react";
import {
  MANAGED_SESSION_EVENT_TYPES,
  MANAGED_SESSION_PREVIEW_TYPES,
  countCommittedEventUpdates,
  isCommittedStreamEvent,
  managedSessionAdmission,
  managedSessionPresentationPhase,
  mergeCommittedEvents,
  projectManagedSessionRuntime,
  type ManagedSessionAdmission,
  type ManagedSessionRuntimeProjection,
  type ManagedSessionStatus,
  type ManagedStreamEvent,
} from "@awaken/managed-session-projection";
import { api, IdempotencyScope, streamUrl } from "./api/client";
import type {
  InboundEvent,
  ListEventsResponse,
  SendEventsResponse,
  SessionEvent,
} from "./api/types";
import { pairToolResults } from "./session-log";

export interface SessionLog {
  log: SessionEvent[];
  results: Map<string, SessionEvent>;
  runtime: ManagedSessionRuntimeProjection;
  admission: ManagedSessionAdmission;
  pendingIds: Set<string>;
  running: boolean;
  freshCount: number;
  applyPending: () => void;
  send: (events: InboundEvent[]) => Promise<SendEventsResponse>;
  /** True from local submit until the Events POST settles. */
  sendPending: boolean;
  sendError: Error | null;
  loadError: Error | null;
  projectionError: Error | null;
  refetch: () => void;
}

export interface SessionLogOptions {
  /** Aggregate-owned status joined conservatively with Event truth. */
  sessionStatus?: ManagedSessionStatus;
  /** Poll and subscribe to SSE (default true). */
  live?: boolean;
  /** Open SSE when live; observers that only share the cache may disable it. */
  subscribe?: boolean;
  /** Apply committed SSE immediately instead of buffering it (default false). */
  followLive?: boolean;
}

function errorOf(cause: unknown): Error {
  return cause instanceof Error ? cause : new Error(String(cause));
}

export function sessionProjectionErrorKey(queryKey: readonly unknown[]): readonly unknown[] {
  return [...queryKey, "projection-error"];
}

function publishProjectionError(
  qc: QueryClient,
  queryKey: readonly unknown[],
  cause: unknown,
): Error {
  const error = errorOf(cause);
  qc.setQueryData<Error>(sessionProjectionErrorKey(queryKey), error);
  return error;
}

/**
 * Canonical application seam for committed HTTP/SSE receipts. It updates the
 * shared query projection synchronously and publishes immutable conflicts to
 * the same fail-closed health key before rethrowing.
 */
export function mergeCommittedSessionCache(
  qc: QueryClient,
  queryKey: readonly unknown[],
  incoming: readonly SessionEvent[],
  source: "observation" | "authoritative-history" = "observation",
): SessionEvent[] {
  try {
    const current = qc.getQueryData<SessionEvent[]>(queryKey) ?? [];
    const merged = mergeCommittedEvents(current, incoming);
    // The official list defaults to chronological order. It is the ordering
    // authority, while the canonical merge above remains the sole identity and
    // maturity policy and therefore retains first-observed event material.
    const projected = source === "authoritative-history"
      ? orderByAuthoritativeHistory(merged, incoming)
      : merged;
    qc.setQueryData<SessionEvent[]>(queryKey, projected);
    return projected;
  } catch (cause) {
    throw publishProjectionError(qc, queryKey, cause);
  }
}

function orderByAuthoritativeHistory(
  merged: SessionEvent[],
  history: readonly SessionEvent[],
): SessionEvent[] {
  const byId = new Map(merged.map((event) => [event.id, event]));
  const ordered: SessionEvent[] = [];
  const seen = new Set<string>();
  for (const event of history) {
    if (seen.has(event.id)) continue;
    const reconciled = byId.get(event.id);
    if (reconciled) ordered.push(reconciled);
    seen.add(event.id);
  }
  for (const event of merged) {
    if (!seen.has(event.id)) ordered.push(event);
  }
  return ordered.length === merged.length
    && ordered.every((event, index) => event === merged[index])
    ? merged
    : ordered;
}

/** Local transport admission cannot fork from the committed projection. */
export function gateManagedSessionAdmissionWhileSending(
  admission: ManagedSessionAdmission,
  sendPending: boolean,
): ManagedSessionAdmission {
  return sendPending
    ? { canSendMessage: false, canResolveTools: false, canInterrupt: false }
    : admission;
}

/**
 * Project any synchronous receipts, then keep the mutation pending until the
 * authoritative list reconciliation finishes. Missing optional `data` cannot
 * create an idle admission window between HTTP completion and refetch.
 */
export async function reconcileSessionSendResponse(
  qc: QueryClient,
  queryKey: readonly unknown[],
  result: SendEventsResponse,
  refetch: () => Promise<unknown>,
): Promise<void> {
  if (result.data) {
    try {
      mergeCommittedSessionCache(qc, queryKey, result.data);
    } catch { /* projection conflict is already published and remains denied */ }
  }
  await refetch();
}

export function useSessionLog(
  base: string,
  queryKey: readonly unknown[],
  options: SessionLogOptions = {},
): SessionLog {
  const { sessionStatus, live = true, subscribe = live, followLive = false } = options;
  const qc = useQueryClient();
  const [pending, setPending] = useState<SessionEvent[]>([]);
  const pendingRef = useRef<SessionEvent[]>([]);
  // One base-bound input owner: route reuse replaces, rather than duplicates,
  // the transport identity before any control can render for the new Session.
  const sendIdentity = useRef<{ base: string; scope: IdempotencyScope } | null>(null);
  if (sendIdentity.current?.base !== base) {
    sendIdentity.current = { base, scope: new IdempotencyScope("session-events") };
  }
  const projectionErrorKey = sessionProjectionErrorKey(queryKey);
  const projectionHealth = useQuery<Error | null>({
    queryKey: projectionErrorKey,
    enabled: false,
    initialData: null,
  });
  const recordProjectionError = (cause: unknown) => {
    return publishProjectionError(qc, queryKey, cause);
  };

  useEffect(() => {
    // A hook instance can survive route changes. Buffered committed facts
    // belong to one Session and must never cross that boundary.
    pendingRef.current = [];
    setPending([]);
  }, [base]);

  const events = useQuery({
    queryKey,
    queryFn: async () => {
      const incoming = (await api.get<ListEventsResponse>(`${base}/events`)).data;
      return mergeCommittedSessionCache(
        qc,
        queryKey,
        incoming,
        "authoritative-history",
      );
    },
    refetchInterval: live ? 5_000 : false,
  });
  const log = useMemo(() => events.data ?? [], [events.data]);

  useEffect(() => {
    if (!subscribe) return;
    const source = new EventSource(streamUrl(`${base}/events/stream`));
    const onAny = (raw: MessageEvent) => {
      let frame: ManagedStreamEvent;
      try {
        frame = JSON.parse(raw.data as string) as ManagedStreamEvent;
      } catch {
        return;
      }
      if (!isCommittedStreamEvent(frame)) return;
      const committed = frame as SessionEvent;
      if (followLive) {
        try {
          mergeCommittedSessionCache(qc, queryKey, [committed]);
        } catch { /* merge seam already published the fail-closed conflict */ }
        return;
      }
      try {
        const current = qc.getQueryData<SessionEvent[]>(queryKey) ?? [];
        if (countCommittedEventUpdates(current, [committed]) === 0) return;
        const next = mergeCommittedEvents(pendingRef.current, [committed]);
        pendingRef.current = next;
        setPending(next);
      } catch (cause) {
        recordProjectionError(cause);
      }
    };
    for (const name of [...MANAGED_SESSION_EVENT_TYPES, ...MANAGED_SESSION_PREVIEW_TYPES]) {
      source.addEventListener(name, onAny);
    }
    source.onerror = () => {
      void qc.invalidateQueries({ queryKey });
    };
    return () => source.close();
  // `queryKey` is deliberately represented by `base`. Inline array identity
  // must not reopen SSE on every committed frame.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [base, followLive, qc, subscribe]);

  const pendingProjection = useMemo(() => {
    try {
      return { count: countCommittedEventUpdates(log, pending), error: null };
    } catch (cause) {
      return { count: 0, error: errorOf(cause) };
    }
  }, [log, pending]);
  useEffect(() => {
    if (pendingProjection.error) {
      qc.setQueryData<Error>(projectionErrorKey, pendingProjection.error);
    }
  // The query key is structurally represented by `base`; depending on the
  // caller's inline array would repeat the same health publication.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [base, pendingProjection.error, qc]);
  const projectionError = projectionHealth.data ?? pendingProjection.error;
  const loadError = projectionError
    ?? (events.error instanceof Error ? events.error : null);
  const runtime = useMemo(() => projectManagedSessionRuntime(log), [log]);
  const projectedAdmission = managedSessionAdmission(runtime, sessionStatus, loadError == null);
  const presentation = managedSessionPresentationPhase(runtime, sessionStatus);

  const applyPending = () => {
    try {
      mergeCommittedSessionCache(qc, queryKey, pendingRef.current);
      pendingRef.current = [];
      setPending([]);
    } catch { /* merge seam already published the fail-closed conflict */ }
  };

  const sendMutation = useMutation({
    mutationFn: ({
      inbound,
      identity,
    }: {
      inbound: InboundEvent[];
      identity: IdempotencyScope;
    }) => {
      const request = { events: inbound };
      return api.post<SendEventsResponse>(
        `${base}/events`,
        request,
        identity.headersFor(request),
      );
    },
    onSuccess: async (result, { identity }) => {
      // HTTP acceptance closes this transport identity even if its receipt
      // proves the local projection inconsistent.
      identity.complete();
      await reconcileSessionSendResponse(qc, queryKey, result, () => events.refetch());
    },
  });
  const admission = gateManagedSessionAdmissionWhileSending(
    projectedAdmission,
    sendMutation.isPending,
  );

  return {
    log,
    results: pairToolResults(log),
    runtime,
    admission,
    pendingIds: runtime.pendingToolIds,
    running: presentation === "running" || presentation === "rescheduling",
    freshCount: pendingProjection.count,
    applyPending,
    send: (inbound) => sendMutation.mutateAsync({
      inbound,
      identity: sendIdentity.current!.scope,
    }),
    sendPending: sendMutation.isPending,
    sendError: sendMutation.error instanceof Error ? sendMutation.error : null,
    loadError,
    projectionError,
    refetch: () => void events.refetch(),
  };
}
