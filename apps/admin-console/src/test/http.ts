import { BACKEND_URL } from "@/lib/config-api";

export function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

export function listResponse(namespace: string, items: unknown[] = []): Response {
  return jsonResponse({ namespace, items, offset: 0, limit: 100 });
}

export function requestUrl(input: string | URL | Request): URL {
  const raw = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
  return new URL(raw);
}

export function requestMethod(init?: RequestInit): string {
  return init?.method?.toUpperCase() ?? "GET";
}

interface FetchSpyLike {
  mock: {
    calls: Array<[string | URL | Request, RequestInit?]>;
  };
}

export function callsFor(fetchSpy: FetchSpyLike, path: string, method = "GET") {
  return fetchSpy.mock.calls.filter(([input, init]) => {
    const url = requestUrl(input);
    return url.href === `${BACKEND_URL}${path}` && requestMethod(init) === method;
  });
}
