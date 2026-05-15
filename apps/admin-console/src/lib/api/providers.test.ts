import { afterEach, describe, expect, it, vi } from "vitest";
import { BACKEND_URL } from "./http";
import { providersApi } from "./providers";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("providersApi", () => {
  it("posts to the provider test endpoint with encoded ids", async () => {
    const payload = { ok: true, latency_ms: 42, network_tested: true };
    const fetchSpy = vi.fn().mockResolvedValue(jsonResponse(payload));
    vi.stubGlobal("fetch", fetchSpy);

    await expect(providersApi.testProvider("vertex/us")).resolves.toEqual(payload);
    expect(fetchSpy).toHaveBeenCalledWith(
      `${BACKEND_URL}/v1/providers/vertex%2Fus/test`,
      { method: "POST" },
    );
  });
});
