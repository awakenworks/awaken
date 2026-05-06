// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, act, screen, fireEvent, waitFor } from "@testing-library/react";
import React from "react";
import { QueryClientProvider } from "@tanstack/react-query";

import { ConfigApiError, configResourceApi } from "./api";
import { useCrudPage } from "./use-crud-page";
import { ConfirmDialogProvider } from "@/components/confirm-dialog";
import { ToastProvider } from "@/components/toast-provider";
import { createAdminQueryClient } from "@/lib/query/client";
import { qk } from "@/lib/query/keys";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

interface SimpleRecord {
  id: string;
}

// Wrapper component that exposes handleDelete via a button click
function TestPage({
  targetId,
  namespace = "providers",
  onDone,
}: {
  targetId: string;
  namespace?: string;
  onDone?: () => void;
}) {
  const { handleDelete, items } = useCrudPage<SimpleRecord>({
    namespace,
    entityLabel: "provider",
  });

  return (
    <div>
      <div data-testid="items">{items.map((i) => i.id).join(",")}</div>
      <button
        data-testid="delete-btn"
        onClick={() => {
          void handleDelete(targetId).then(onDone);
        }}
      >
        Delete
      </button>
    </div>
  );
}

function renderTest(
  targetId: string,
  onDone?: () => void,
  options: { client?: ReturnType<typeof createAdminQueryClient>; namespace?: string } = {},
) {
  const client = options.client ?? createAdminQueryClient();
  return render(
    <QueryClientProvider client={client}>
      <ToastProvider>
        <ConfirmDialogProvider>
          <TestPage targetId={targetId} namespace={options.namespace} onDone={onDone} />
        </ConfirmDialogProvider>
      </ToastProvider>
      ,
    </QueryClientProvider>,
  );
}

describe("useCrudPage handleDelete 409 → force flow", () => {
  it("re-prompts with used_by list when delete returns 409 and force-deletes on confirm", async () => {
    // First delete call: 409 with used_by
    const deleteStub = vi
      .spyOn(configResourceApi, "delete")
      .mockRejectedValueOnce(
        new ConfigApiError(409, {
          error: "blocked",
          used_by: [{ namespace: "models", id: "model-a" }],
        }),
      )
      // Force delete call: success
      .mockResolvedValueOnce(undefined);

    vi.spyOn(configResourceApi, "list").mockResolvedValue({
      namespace: "providers",
      items: [{ id: "prov-a" }],
      offset: 0,
      limit: 100,
    });

    renderTest("prov-a");

    // Click delete button
    await act(async () => {
      fireEvent.click(screen.getByTestId("delete-btn"));
    });

    // First dialog: "Delete provider?" should appear
    expect(screen.getByRole("dialog")).toBeTruthy();
    expect(screen.getByText(/Delete provider\?/)).toBeTruthy();

    // Confirm initial delete — click the confirm button inside the dialog
    await act(async () => {
      const dialog = screen.getByRole("dialog");
      const confirmBtn = dialog.querySelector<HTMLButtonElement>("button:last-child");
      fireEvent.click(confirmBtn!);
    });

    // Second dialog: "Still delete?" with used_by list. The list now groups
    // by namespace so "models" appears as a header and "model-a" as a chip.
    expect(screen.getByText(/Still delete\?/)).toBeTruthy();
    expect(screen.getByText(/^models$/)).toBeTruthy();
    expect(screen.getByText("model-a")).toBeTruthy();

    // Confirm force delete (button, not the inline strong text in body)
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Force delete" }));
    });

    // delete called twice: first without force, then with force
    expect(deleteStub).toHaveBeenCalledTimes(2);
    expect(deleteStub.mock.calls[0]).toEqual(["providers", "prov-a"]);
    expect(deleteStub.mock.calls[1]).toEqual(["providers", "prov-a", { force: true }]);
  });

  it("cancels without force-delete when user dismisses the second dialog", async () => {
    const deleteStub = vi.spyOn(configResourceApi, "delete").mockRejectedValueOnce(
      new ConfigApiError(409, {
        error: "blocked",
        used_by: [{ namespace: "models", id: "model-b" }],
      }),
    );

    vi.spyOn(configResourceApi, "list").mockResolvedValue({
      namespace: "providers",
      items: [{ id: "prov-b" }],
      offset: 0,
      limit: 100,
    });

    renderTest("prov-b");

    await act(async () => {
      fireEvent.click(screen.getByTestId("delete-btn"));
    });

    // Confirm initial delete — click the confirm button inside the dialog
    await act(async () => {
      const dialog = screen.getByRole("dialog");
      const confirmBtn = dialog.querySelector<HTMLButtonElement>("button:last-child");
      fireEvent.click(confirmBtn!);
    });

    // Second dialog appears
    expect(screen.getByText(/Still delete\?/)).toBeTruthy();

    // Cancel — first button in the second dialog
    await act(async () => {
      fireEvent.click(screen.getByText("Cancel"));
    });

    // Force delete should NOT have been called
    expect(deleteStub).toHaveBeenCalledTimes(1);
  });

  it("reports error for non-409 errors without re-prompting", async () => {
    vi.spyOn(configResourceApi, "delete").mockRejectedValueOnce(
      new ConfigApiError(500, { error: "internal error" }),
    );

    vi.spyOn(configResourceApi, "list").mockResolvedValue({
      namespace: "providers",
      items: [{ id: "prov-c" }],
      offset: 0,
      limit: 100,
    });

    renderTest("prov-c");

    await act(async () => {
      fireEvent.click(screen.getByTestId("delete-btn"));
    });

    // Confirm initial delete — click the confirm button inside the dialog
    await act(async () => {
      const dialog = screen.getByRole("dialog");
      const confirmBtn = dialog.querySelector<HTMLButtonElement>("button:last-child");
      fireEvent.click(confirmBtn!);
    });

    // "Still delete?" should NOT appear
    expect(screen.queryByText(/Still delete\?/)).toBeNull();
  });

  it("removes deleted detail caches and invalidates dependent summaries", async () => {
    const client = createAdminQueryClient();
    client.setQueryData(qk.config.get("providers", "prov-d"), { id: "prov-d" });
    client.setQueryData(qk.config.meta("providers", "prov-d"), {
      source: { kind: "user" },
      user_overrides: null,
      hidden: false,
      created_at: 0,
      updated_at: 0,
    });
    client.setQueryData(qk.config.listMeta("providers"), []);
    client.setQueryData(qk.navHealth(), {});
    client.setQueryData(qk.dashboard("24h"), {});
    client.setQueryData(qk.capabilities(), {});

    const deleteStub = vi.spyOn(configResourceApi, "delete").mockResolvedValue(undefined);
    vi.spyOn(configResourceApi, "list").mockResolvedValue({
      namespace: "providers",
      items: [{ id: "prov-d" }],
      offset: 0,
      limit: 100,
    });

    renderTest("prov-d", undefined, { client });

    await act(async () => {
      fireEvent.click(screen.getByTestId("delete-btn"));
    });
    await act(async () => {
      const dialog = screen.getByRole("dialog");
      const confirmBtn = dialog.querySelector<HTMLButtonElement>("button:last-child");
      fireEvent.click(confirmBtn!);
    });

    await waitFor(() => expect(deleteStub).toHaveBeenCalledWith("providers", "prov-d"));
    expect(client.getQueryData(qk.config.get("providers", "prov-d"))).toBeUndefined();
    expect(client.getQueryData(qk.config.meta("providers", "prov-d"))).toBeUndefined();
    expect(client.getQueryState(qk.config.listMeta("providers"))?.isInvalidated).toBe(true);
    expect(client.getQueryState(qk.navHealth())?.isInvalidated).toBe(true);
    expect(client.getQueryState(qk.dashboard("24h"))?.isInvalidated).toBe(true);
    expect(client.getQueryState(qk.capabilities())?.isInvalidated).toBe(true);
  });
});
