import { type AuditPage, type AuditQuery, buildAuditQueryString } from "../audit-log";
import { BACKEND_URL, ConfigApiError, fetchJson } from "./http";

export const auditApi = {
  auditLog: async (query: AuditQuery): Promise<AuditPage> => {
    const qs = buildAuditQueryString(query).toString();
    const url = `${BACKEND_URL}/v1/audit-log${qs ? `?${qs}` : ""}`;
    try {
      return await fetchJson<AuditPage>(url);
    } catch (err) {
      if (err instanceof ConfigApiError && err.status === 503) {
        throw new ConfigApiError(503, "audit log not configured");
      }
      throw err;
    }
  },
};
