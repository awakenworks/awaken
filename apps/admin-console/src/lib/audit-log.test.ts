// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  buildAuditQueryString,
  formatActor,
  summarizeChange,
  type AuditEvent,
} from "./audit-log";
import { BACKEND_URL, ConfigApiError, configApi } from "./config-api";

// ---------------------------------------------------------------------------
// buildAuditQueryString
// ---------------------------------------------------------------------------

describe("buildAuditQueryString", () => {
  it("returns empty params when query is empty", () => {
    const params = buildAuditQueryString({});
    expect(params.toString()).toBe("");
  });

  it("includes since and until when provided", () => {
    const params = buildAuditQueryString({
      since: "2026-01-01T00:00:00Z",
      until: "2026-02-01T00:00:00Z",
    });
    expect(params.get("since")).toBe("2026-01-01T00:00:00Z");
    expect(params.get("until")).toBe("2026-02-01T00:00:00Z");
  });

  it("includes action filter", () => {
    const params = buildAuditQueryString({ action: "delete" });
    expect(params.get("action")).toBe("delete");
  });

  it("includes resource filter", () => {
    const params = buildAuditQueryString({ resource: "agents/my-agent" });
    expect(params.get("resource")).toBe("agents/my-agent");
  });

  it("includes actor filter", () => {
    const params = buildAuditQueryString({ actor: "abc123" });
    expect(params.get("actor")).toBe("abc123");
  });

  it("includes limit and cursor", () => {
    const params = buildAuditQueryString({ limit: 50, cursor: "abc" });
    expect(params.get("limit")).toBe("50");
    expect(params.get("cursor")).toBe("abc");
  });

  it("omits undefined fields", () => {
    const params = buildAuditQueryString({ action: "create" });
    expect(params.has("since")).toBe(false);
    expect(params.has("until")).toBe(false);
    expect(params.has("resource")).toBe(false);
    expect(params.has("cursor")).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// formatActor
// ---------------------------------------------------------------------------

describe("formatActor", () => {
  it("returns label null for bare hash", () => {
    const result = formatActor("abc123def456789a");
    expect(result.hash).toBe("abc123def456789a");
    expect(result.label).toBeNull();
  });

  it("splits hash/label on first slash", () => {
    const result = formatActor("abc123/ci/deploy-prod");
    expect(result.hash).toBe("abc123");
    expect(result.label).toBe("ci/deploy-prod");
  });

  it("handles anonymous actor", () => {
    const result = formatActor("anonymous");
    expect(result.hash).toBe("anonymous");
    expect(result.label).toBeNull();
  });

  it("handles hash/label where label has no slashes", () => {
    const result = formatActor("deadbeef12345678/admin");
    expect(result.hash).toBe("deadbeef12345678");
    expect(result.label).toBe("admin");
  });
});

// ---------------------------------------------------------------------------
// summarizeChange
// ---------------------------------------------------------------------------

function makeEvent(
  action: AuditEvent["action"],
  before?: Record<string, unknown> | null,
  after?: Record<string, unknown> | null,
): AuditEvent {
  return {
    id: "01J",
    ts: "2026-01-01T00:00:00Z",
    actor: "anonymous",
    action,
    resource: "agents/x",
    before,
    after,
  };
}

describe("summarizeChange", () => {
  it("returns Created for create action", () => {
    expect(summarizeChange(makeEvent("create", null, { id: "x" }))).toBe("Created");
  });

  it("returns Deleted for delete action", () => {
    expect(summarizeChange(makeEvent("delete", { id: "x" }, null))).toBe("Deleted");
  });

  it("returns Restarted for restart action", () => {
    expect(summarizeChange(makeEvent("restart"))).toBe("Restarted");
  });

  it("returns Published for publish action", () => {
    expect(summarizeChange(makeEvent("publish"))).toBe("Published");
  });

  it("returns Updated for update with no before/after", () => {
    expect(summarizeChange(makeEvent("update"))).toBe("Updated");
  });

  it("lists changed fields for update with before/after", () => {
    const result = summarizeChange(
      makeEvent("update", { model_id: "gpt-4", name: "x" }, { model_id: "gpt-4o", name: "x" }),
    );
    expect(result).toContain("model_id");
  });

  it("lists added fields for update", () => {
    const result = summarizeChange(
      makeEvent("update", { model_id: "gpt-4" }, { model_id: "gpt-4", new_field: "v" }),
    );
    expect(result).toContain("new_field");
  });

  it("lists removed fields for update", () => {
    const result = summarizeChange(
      makeEvent("update", { model_id: "gpt-4", old_field: "v" }, { model_id: "gpt-4" }),
    );
    expect(result).toContain("old_field");
  });

  it("returns restored from <prefix> for restore event", () => {
    const event: AuditEvent = {
      ...makeEvent("restore"),
      restored_from: "01JXYZ12345678abcdef",
    };
    expect(summarizeChange(event)).toBe("restored from 01JXYZ12");
  });

  it("returns restored from unknown for restore event with null restored_from", () => {
    const event: AuditEvent = {
      ...makeEvent("restore"),
      restored_from: null,
    };
    expect(summarizeChange(event)).toBe("restored from unknown");
  });

  it("returns restored from unknown for restore event with no restored_from", () => {
    expect(summarizeChange(makeEvent("restore"))).toBe("restored from unknown");
  });
});

// ---------------------------------------------------------------------------
// AuditPage shape parsing
// ---------------------------------------------------------------------------

describe("AuditPage shape", () => {
  it("deserializes correctly from JSON", () => {
    const raw = {
      items: [
        {
          id: "01JXYZ",
          ts: "2026-01-01T00:00:00Z",
          actor: "abc123",
          action: "create",
          resource: "agents/foo",
          before: null,
          after: { id: "foo" },
          ip: "1.2.3.4",
          request_id: "req-1",
        },
      ],
      next_cursor: "abc",
    };
    // Shape check — ensure fields are accessible as typed
    const page = raw as import("./audit-log").AuditPage;
    expect(page.items).toHaveLength(1);
    expect(page.items[0].action).toBe("create");
    expect(page.items[0].resource).toBe("agents/foo");
    expect(page.next_cursor).toBe("abc");
  });

  it("handles page with no next_cursor", () => {
    const raw = { items: [] };
    const page = raw as import("./audit-log").AuditPage;
    expect(page.next_cursor).toBeUndefined();
    expect(page.items).toHaveLength(0);
  });
});

// ---------------------------------------------------------------------------
// configApi.auditLog
// ---------------------------------------------------------------------------

describe("configApi.auditLog", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("calls GET /v1/audit-log with no params when query empty", async () => {
    const fetchSpy = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: async () => JSON.stringify({ items: [], next_cursor: undefined }),
    });
    vi.stubGlobal("fetch", fetchSpy);

    await configApi.auditLog({});

    const calledUrl: string = fetchSpy.mock.calls[0][0] as string;
    expect(calledUrl).toBe(`${BACKEND_URL}/v1/audit-log`);
  });

  it("serializes resource filter into query string", async () => {
    const fetchSpy = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: async () => JSON.stringify({ items: [] }),
    });
    vi.stubGlobal("fetch", fetchSpy);

    await configApi.auditLog({ resource: "agents/my-agent" });

    const calledUrl: string = fetchSpy.mock.calls[0][0] as string;
    expect(calledUrl).toContain("resource=agents%2Fmy-agent");
  });

  it("throws ConfigApiError with status 400 on bad request", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: false,
        status: 400,
        text: async () => JSON.stringify({ error: "invalid cursor" }),
      }),
    );

    await expect(configApi.auditLog({ cursor: "bad" })).rejects.toMatchObject({
      status: 400,
    });
  });

  it("throws ConfigApiError with detail 'audit log not configured' on 503", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: false,
        status: 503,
        text: async () => "Service Unavailable",
      }),
    );

    const err = await configApi.auditLog({}).catch((e) => e);
    expect(err).toBeInstanceOf(ConfigApiError);
    expect((err as ConfigApiError).status).toBe(503);
    expect((err as ConfigApiError).message).toContain("audit log not configured");
  });
});
