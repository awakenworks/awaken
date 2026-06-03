// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ConfirmDialogProvider } from "@/components/confirm-dialog";
import { ToastProvider } from "@/components/toast-provider";
import { withQueryClient } from "@/test/query";
import { A2aServersPage } from "./a2a-servers-page";

function jsonResponse(data: unknown, init?: { ok?: boolean; status?: number }) {
  return {
    ok: init?.ok ?? true,
    status: init?.status ?? 200,
    text: async () => JSON.stringify(data),
  };
}

function fetchHref(input: string | URL | Request): string {
  if (typeof input === "string") return input;
  if (input instanceof URL) return input.href;
  return input.url;
}

function renderPage() {
  return render(
    withQueryClient(
      <MemoryRouter>
        <ToastProvider>
          <ConfirmDialogProvider>
            <A2aServersPage />
          </ConfirmDialogProvider>
        </ToastProvider>
      </MemoryRouter>,
    ),
  );
}

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("A2A servers page", () => {
  it("renders status errors returned by the A2A status endpoint", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: string | URL | Request) => {
        const href = fetchHref(input);
        if (href.includes("/v1/config/a2a-servers")) {
          return jsonResponse({
            items: [
              {
                id: "partner",
                base_url: "https://partner.example.com/a2a",
                timeout_ms: 10_000,
                options: {},
              },
            ],
          });
        }
        if (href.includes("/v1/a2a-servers/partner/status")) {
          return jsonResponse({
            connected: false,
            last_error: "invalid agent card JSON",
            card_url: null,
            card: null,
          });
        }
        throw new Error(`Unexpected fetch ${href}`);
      }),
    );

    renderPage();

    expect(await screen.findByText("partner")).toBeTruthy();
    expect(await screen.findByText("invalid agent card JSON")).toBeTruthy();
  });

  it("creates A2A servers with default timeout, target, and options", async () => {
    const fetchMock = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const href = fetchHref(input);
      const method = init?.method?.toUpperCase() ?? "GET";
      if (method === "GET" && href.includes("/v1/config/a2a-servers")) {
        return jsonResponse({ items: [] });
      }
      if (method === "POST" && href.includes("/v1/config/a2a-servers")) {
        return jsonResponse(JSON.parse(String(init?.body)), { status: 201 });
      }
      throw new Error(`Unexpected fetch ${method} ${href}`);
    });
    vi.stubGlobal("fetch", fetchMock);

    renderPage();
    await screen.findByText("No A2A servers configured");
    fireEvent.click(screen.getAllByRole("button", { name: "New A2A server" })[0]);
    fireEvent.change(screen.getByLabelText("Server ID"), { target: { value: "partner" } });
    fireEvent.change(screen.getByLabelText("Base URL"), {
      target: { value: "https://partner.example.com/a2a" },
    });
    fireEvent.change(screen.getByLabelText("Optional target"), {
      target: { value: "assistant" },
    });
    fireEvent.change(screen.getByLabelText("Options JSON"), {
      target: { value: '{"region":"us-east"}' },
    });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => {
      expect(
        fetchMock.mock.calls.some(([input, init]) => {
          const href = fetchHref(input as string | URL | Request);
          return init?.method === "POST" && href.includes("/v1/config/a2a-servers");
        }),
      ).toBe(true);
    });
    const [, init] = fetchMock.mock.calls.find(([input, init]) => {
      const href = fetchHref(input as string | URL | Request);
      return init?.method === "POST" && href.includes("/v1/config/a2a-servers");
    })!;
    expect(JSON.parse(String(init?.body))).toMatchObject({
      id: "partner",
      base_url: "https://partner.example.com/a2a",
      target: "assistant",
      timeout_ms: 10_000,
      options: { region: "us-east" },
    });
  });
});
