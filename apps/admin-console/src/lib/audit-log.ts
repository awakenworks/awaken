export type AuditAction = "create" | "update" | "delete" | "restart" | "publish" | "restore";

export interface AuditEvent {
  id: string;
  ts: string; // RFC 3339
  actor: string;
  action: AuditAction;
  resource: string; // "<namespace>/<id>"
  before?: Record<string, unknown> | null;
  after?: Record<string, unknown> | null;
  ip?: string | null;
  request_id?: string | null;
  restored_from?: string | null;
}

export interface AuditQuery {
  since?: string;
  until?: string;
  action?: AuditAction;
  resource?: string;
  actor?: string;
  limit?: number;
  cursor?: string;
}

export interface AuditPage {
  items: AuditEvent[];
  next_cursor?: string;
}

/** Build a URLSearchParams from an AuditQuery, omitting undefined/empty fields. */
export function buildAuditQueryString(query: AuditQuery): URLSearchParams {
  const params = new URLSearchParams();
  if (query.since) params.set("since", query.since);
  if (query.until) params.set("until", query.until);
  if (query.action) params.set("action", query.action);
  if (query.resource) params.set("resource", query.resource);
  if (query.actor) params.set("actor", query.actor);
  if (query.limit !== undefined) params.set("limit", String(query.limit));
  if (query.cursor) params.set("cursor", query.cursor);
  return params;
}

/**
 * Pretty-print an actor string.
 * "abc123/label" → { hash: "abc123", label: "label" }
 * "anonymous"    → { hash: "anonymous", label: null }
 */
export function formatActor(actor: string): { hash: string; label: string | null } {
  const slash = actor.indexOf("/");
  if (slash === -1) {
    return { hash: actor, label: null };
  }
  return { hash: actor.slice(0, slash), label: actor.slice(slash + 1) };
}

/** Heuristic: actor label looks like an agent id when present and not an
 *  email address. Used to apply the agent-identity tint across audit log,
 *  activity timeline, and assistant chat. */
export function isAgentActor(actor: string | null | undefined): boolean {
  if (!actor) return false;
  const { label } = formatActor(actor);
  if (!label) return false;
  if (label.includes("@")) return false;
  if (label === "system") return false;
  return true;
}

/** Short human-readable summary for the change column of an audit table row. */
export function summarizeChange(event: AuditEvent): string {
  if (event.action === "restore") {
    const sourcePrefix = event.restored_from?.slice(0, 8) ?? "unknown";
    return `restored from ${sourcePrefix}`;
  }

  const hasBefore = event.before != null;
  const hasAfter = event.after != null;

  switch (event.action) {
    case "create":
      return hasAfter ? "Created" : "Created";
    case "delete":
      return hasBefore ? "Deleted" : "Deleted";
    case "update":
      if (hasBefore && hasAfter) {
        const beforeKeys = Object.keys(event.before as Record<string, unknown>);
        const afterKeys = Object.keys(event.after as Record<string, unknown>);
        const changed = afterKeys.filter(
          (k) =>
            JSON.stringify((event.after as Record<string, unknown>)[k]) !==
            JSON.stringify((event.before as Record<string, unknown>)[k]),
        );
        const added = afterKeys.filter((k) => !beforeKeys.includes(k));
        const removed = beforeKeys.filter((k) => !afterKeys.includes(k));
        const parts: string[] = [];
        if (changed.length > 0) parts.push(`updated ${changed.join(", ")}`);
        if (added.length > 0) parts.push(`added ${added.join(", ")}`);
        if (removed.length > 0) parts.push(`removed ${removed.join(", ")}`);
        return parts.length > 0 ? parts.join("; ") : "Updated";
      }
      return "Updated";
    case "restart":
      return "Restarted";
    case "publish":
      return "Published";
  }
}
