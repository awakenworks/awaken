import { useInfiniteQuery } from "@tanstack/react-query";
import { auditApi } from "../../api";
import type { AuditPage, AuditQuery } from "../../audit-log";
import { qk } from "../keys";

export function useAuditLogInfiniteQuery(query: AuditQuery, options: { enabled?: boolean } = {}) {
  return useInfiniteQuery<AuditPage>({
    queryKey: qk.audit.log(query),
    queryFn: ({ pageParam }) =>
      auditApi.auditLog({
        ...query,
        cursor: typeof pageParam === "string" ? pageParam : undefined,
      }),
    initialPageParam: undefined,
    getNextPageParam: (lastPage) => lastPage.next_cursor,
    enabled: options.enabled ?? true,
  });
}
