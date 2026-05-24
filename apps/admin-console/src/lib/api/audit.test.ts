import { afterEach, describe, expect, it, vi } from "vitest";
import { auditApi } from "./audit";
import { ConfigApiError } from "./http";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function mockFetch(status: number, body: unknown) {
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(jsonResponse(body, status)));
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("auditApi.auditLog", () => {
  it("returns the page on success", async () => {
    const page = { items: [], next_cursor: null };
    mockFetch(200, page);
    await expect(auditApi.auditLog({})).resolves.toEqual(page);
  });

  it("normalises 503 → ConfigApiError(503, 'audit log not configured')", async () => {
    mockFetch(503, { error: "audit logger not wired" });
    await expect(auditApi.auditLog({})).rejects.toMatchObject({
      name: "ConfigApiError",
      status: 503,
      message: "audit log not configured",
    });
  });

  it("normalises 404 → ConfigApiError(503, 'audit log not configured')", async () => {
    // Older deploys / partial rollouts predate /v1/audit-log. The
    // downstream code (dashboard auditPromise + audit-log-page) only
    // special-cases 503, so we re-throw as 503 here to keep one shape
    // for "feature absent" instead of leaking a raw 404 into the UI.
    mockFetch(404, { error: "not found" });
    await expect(auditApi.auditLog({})).rejects.toMatchObject({
      name: "ConfigApiError",
      status: 503,
      message: "audit log not configured",
    });
  });

  it("rethrows auth and other errors unchanged", async () => {
    mockFetch(401, { error: "unauthorized" });
    await expect(auditApi.auditLog({})).rejects.toMatchObject({
      name: "ConfigApiError",
      status: 401,
    });
  });

  it("preserves ConfigApiError type for all surfaced errors", async () => {
    mockFetch(404, { error: "missing" });
    try {
      await auditApi.auditLog({});
      expect.fail("expected rejection");
    } catch (err) {
      expect(err).toBeInstanceOf(ConfigApiError);
    }
  });
});
