import { afterEach, describe, expect, it, vi } from "vitest";
import { configResourceApi } from "./config-resource";
import { BACKEND_URL } from "./http";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("configResourceApi", () => {
  it("lists and fetches config records with encoded ids", async () => {
    const fetchSpy = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse({ namespace: "agents", items: [] }))
      .mockResolvedValueOnce(jsonResponse({ id: "team/a" }));
    vi.stubGlobal("fetch", fetchSpy);

    await configResourceApi.list("agents", 25, 50);
    await configResourceApi.get("agents", "team/a");

    expect(fetchSpy.mock.calls[0]).toEqual([
      `${BACKEND_URL}/v1/config/agents?offset=25&limit=50`,
      undefined,
    ]);
    expect(fetchSpy.mock.calls[1]).toEqual([
      `${BACKEND_URL}/v1/config/agents/team%2Fa`,
      undefined,
    ]);
  });

  it("creates and updates config records with JSON payloads", async () => {
    const fetchSpy = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse({ id: "agent/a" }))
      .mockResolvedValueOnce(jsonResponse({ id: "agent/a", model_id: "fast" }));
    vi.stubGlobal("fetch", fetchSpy);

    await configResourceApi.create("agents", { id: "agent/a" });
    await configResourceApi.update("agents", "agent/a", { model_id: "fast" });

    expect(fetchSpy.mock.calls[0][0]).toBe(`${BACKEND_URL}/v1/config/agents`);
    expect(fetchSpy.mock.calls[0][1]).toMatchObject({
      method: "POST",
      body: JSON.stringify({ id: "agent/a" }),
    });
    expect(new Headers((fetchSpy.mock.calls[0][1] as RequestInit).headers).get("content-type")).toBe(
      "application/json",
    );
    expect(fetchSpy.mock.calls[1][0]).toBe(`${BACKEND_URL}/v1/config/agents/agent%2Fa`);
    expect(fetchSpy.mock.calls[1][1]).toMatchObject({
      method: "PUT",
      body: JSON.stringify({ model_id: "fast" }),
    });
  });

  it("deletes records with optional force", async () => {
    const fetchSpy = vi
      .fn()
      .mockResolvedValueOnce(new Response(null, { status: 204 }))
      .mockResolvedValueOnce(new Response(null, { status: 204 }));
    vi.stubGlobal("fetch", fetchSpy);

    await configResourceApi.delete("agents", "agent/a");
    await configResourceApi.delete("agents", "agent/a", { force: true });

    expect(fetchSpy.mock.calls[0]).toEqual([
      `${BACKEND_URL}/v1/config/agents/agent%2Fa`,
      { method: "DELETE" },
    ]);
    expect(fetchSpy.mock.calls[1]).toEqual([
      `${BACKEND_URL}/v1/config/agents/agent%2Fa?force=true`,
      { method: "DELETE" },
    ]);
  });

  it("validates and restores records with encoded query/path parameters", async () => {
    const fetchSpy = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse({ ok: true, normalized: { id: "agent/a" } }))
      .mockResolvedValueOnce(jsonResponse({ id: "agent/a", restored: true }));
    vi.stubGlobal("fetch", fetchSpy);

    await configResourceApi.validateConfig("agents/special", { id: "agent/a" }, { id: "agent/a" });
    await configResourceApi.restoreConfig("agents/special", "agent/a", "event/1");

    expect(fetchSpy.mock.calls[0][0]).toBe(
      `${BACKEND_URL}/v1/config/agents%2Fspecial/validate?id=agent%2Fa`,
    );
    expect(fetchSpy.mock.calls[0][1]).toMatchObject({
      method: "POST",
      body: JSON.stringify({ id: "agent/a" }),
    });
    expect(fetchSpy.mock.calls[1][0]).toBe(
      `${BACKEND_URL}/v1/config/agents%2Fspecial/agent%2Fa/restore`,
    );
    expect(fetchSpy.mock.calls[1][1]).toMatchObject({
      method: "POST",
      body: JSON.stringify({ version: "event/1" }),
    });
  });

  it("loads record metadata endpoints", async () => {
    const meta = {
      source: { kind: "user" },
      hidden: false,
      created_at: 1,
      updated_at: 2,
    };
    const fetchSpy = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse(meta))
      .mockResolvedValueOnce(jsonResponse([{ id: "agent/a", meta }]));
    vi.stubGlobal("fetch", fetchSpy);

    await expect(configResourceApi.getMeta("agents", "agent/a")).resolves.toEqual(meta);
    await expect(configResourceApi.listMeta("agents")).resolves.toEqual([{ id: "agent/a", meta }]);
    expect(fetchSpy.mock.calls[0][0]).toBe(`${BACKEND_URL}/v1/config/agents/agent%2Fa/meta`);
    expect(fetchSpy.mock.calls[1][0]).toBe(`${BACKEND_URL}/v1/config/agents/meta`);
  });
});
