// Truth-driven capability gate: probe a real endpoint and reduce the query to a
// four-way state. A face that is absent (404/405) is genuinely not mounted — a
// designed surface waiting on its backend, not an error. Live → render the real
// UI; absent → a gate banner. Shared by every surface that fronts a face which
// may not be mounted yet (eval, audit, history, spans, sandbox, assistant).

import { useQuery } from "@tanstack/react-query";
import { api, isAbsent, ws } from "./api/client";

export type GateState<T> =
  | { status: "loading" }
  | { status: "absent" }
  | { status: "error"; error: Error }
  | { status: "live"; data: T };

/** Reduce a react-query-like result to a GateState. Pure, so it is unit-tested
 * without a QueryClient. */
export function toGateState<T>(q: {
  isLoading: boolean;
  isError: boolean;
  error: unknown;
  data: T | undefined;
}): GateState<T> {
  if (q.isLoading) return { status: "loading" };
  if (q.isError) {
    if (isAbsent(q.error)) return { status: "absent" };
    return { status: "error", error: q.error instanceof Error ? q.error : new Error("error") };
  }
  return { status: "live", data: q.data as T };
}

/** Probe `path` (workspace-scoped via ws()) and expose it as a GateState. */
export function useGate<T = unknown>(path: string): GateState<T> & { refetch: () => void } {
  const query = useQuery({
    queryKey: ["gate", path],
    queryFn: () => api.get<T>(ws(path)),
    retry: (n, err) => !isAbsent(err) && n < 1,
  });
  return { ...toGateState<T>(query), refetch: () => void query.refetch() };
}
