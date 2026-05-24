import { type AuditPage, type AuditQuery, buildAuditQueryString } from "../audit-log";
import { BACKEND_URL, ConfigApiError, fetchJson } from "./http";

export const auditApi = {
  auditLog: async (query: AuditQuery): Promise<AuditPage> => {
    const qs = buildAuditQueryString(query).toString();
    const url = `${BACKEND_URL}/v1/audit-log${qs ? `?${qs}` : ""}`;
    try {
      return await fetchJson<AuditPage>(url);
    } catch (err) {
      // 503 = the runtime didn't wire an audit logger into AppState.
      // 404 = older deploy / partial rollout that predates the route.
      // Both map to the same "not configured" notice downstream — the
      // operator's remedy is identical (enable the subsystem). Doing
      // the normalisation here keeps the dashboard + audit page from
      // each having to special-case two equivalent shapes.
      if (
        err instanceof ConfigApiError &&
        (err.status === 503 || err.status === 404)
      ) {
        throw new ConfigApiError(503, "audit log not configured");
      }
      throw err;
    }
  },
};
