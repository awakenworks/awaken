// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  ADMIN_TOKEN_STORAGE_KEY,
  adminAuthHeaders,
  ConfigApiError,
  fetchWithAdminAuth,
} from "./http";

afterEach(() => {
  vi.restoreAllMocks();
  globalThis.localStorage.removeItem(ADMIN_TOKEN_STORAGE_KEY);
});

describe("adminAuthHeaders (R11 #1 / #2)", () => {
  it("returns an empty object when no token is configured", () => {
    expect(adminAuthHeaders()).toEqual({});
  });

  it("emits a Bearer header when localStorage has a token", () => {
    globalThis.localStorage.setItem(ADMIN_TOKEN_STORAGE_KEY, "my-token");
    expect(adminAuthHeaders()).toEqual({ authorization: "Bearer my-token" });
  });

  it("trims whitespace around the stored token", () => {
    globalThis.localStorage.setItem(ADMIN_TOKEN_STORAGE_KEY, "  spaced  ");
    expect(adminAuthHeaders()).toEqual({ authorization: "Bearer spaced" });
  });
});

describe("fetchWithAdminAuth (R11 #2)", () => {
  it("attaches the admin Bearer header from localStorage", async () => {
    globalThis.localStorage.setItem(ADMIN_TOKEN_STORAGE_KEY, "my-token");
    const fetchMock = vi.fn(async () => ({
      ok: true,
      status: 200,
      headers: new Headers(),
      text: async () => "",
    })) as unknown as typeof fetch;
    vi.stubGlobal("fetch", fetchMock);

    await fetchWithAdminAuth("https://example.com/api");

    const [, init] = (fetchMock as unknown as { mock: { calls: [string, RequestInit][] } }).mock
      .calls[0];
    const headers = new Headers(init?.headers);
    expect(headers.get("authorization")).toBe("Bearer my-token");
  });

  it("returns the raw Response (no body parsing) — NDJSON-friendly", async () => {
    const expected = {
      ok: true,
      status: 200,
      headers: new Headers({ "x-custom": "yes" }),
      text: async () => "stream line 1\nstream line 2",
    } as unknown as Response;
    vi.stubGlobal("fetch", vi.fn(async () => expected) as unknown as typeof fetch);

    const response = await fetchWithAdminAuth("https://example.com/api");
    expect(response).toBe(expected);
    expect(response.headers.get("x-custom")).toBe("yes");
    // Body is NOT consumed by fetchWithAdminAuth — the caller decides.
    const body = await response.text();
    expect(body).toBe("stream line 1\nstream line 2");
  });

  // Sanity-check that ConfigApiError isn't auto-thrown — fetchWithAdminAuth
  // returns the Response regardless of status so streaming callers can
  // inspect status themselves (e.g. for 503 -> "feature unavailable").
  it("does not throw on non-2xx — returns the response unchanged", async () => {
    const errored = {
      ok: false,
      status: 503,
      headers: new Headers(),
      text: async () => "service unavailable",
    } as unknown as Response;
    vi.stubGlobal("fetch", vi.fn(async () => errored) as unknown as typeof fetch);

    const response = await fetchWithAdminAuth("https://example.com/api");
    expect(response.status).toBe(503);
  });
});

// ConfigApiError sanity check — guards against accidental rename.
describe("ConfigApiError shape", () => {
  it("carries status + detail and renders a usable message", () => {
    const err = new ConfigApiError(418, { error: "I'm a teapot" });
    expect(err.status).toBe(418);
    expect(err.message).toBe("I'm a teapot");
    expect(err.detail).toEqual({ error: "I'm a teapot" });
  });

  it("redacts credential patterns from response error messages", () => {
    const err = new ConfigApiError(500, {
      error: "upstream failed with Authorization: Bearer sk-real-secret-value",
    });

    expect(err.message).toContain("Authorization: ***");
    expect(err.message).not.toContain("sk-real-secret-value");
  });

  it("uses and redacts a response message field when error is absent", () => {
    const err = new ConfigApiError(500, {
      message: "upstream failed with api_key=raw-api-key-value",
    });

    expect(err.message).toContain("api_key=***");
    expect(err.message).not.toContain("raw-api-key-value");
  });
});
