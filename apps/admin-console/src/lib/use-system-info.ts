import { useSystemInfoQuery } from "./query/hooks/system";
import type { SystemInfo } from "./config-api";

/** Cached system info backed by the shared QueryClient. */
export function useSystemInfo(): SystemInfo | null {
  return useSystemInfoQuery().data ?? null;
}
