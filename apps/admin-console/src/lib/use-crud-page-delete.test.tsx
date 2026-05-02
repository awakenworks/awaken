// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, act, screen, fireEvent } from "@testing-library/react";
import React, { useEffect } from "react";

import { ConfigApiError, configApi } from "./config-api";
import { useCrudPage } from "./use-crud-page";
import { ConfirmDialogProvider } from "@/components/confirm-dialog";
import { ToastProvider } from "@/components/toast-provider";

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
  onDone,
}: {
  targetId: string;
  onDone?: () => void;
}) {
  const { handleDelete, items } = useCrudPage<SimpleRecord>({
    namespace: "providers",
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

function renderTest(targetId: string, onDone?: () => void) {
  return render(
    <ToastProvider>
      <ConfirmDialogProvider>
        <TestPage targetId={targetId} onDone={onDone} />
      </ConfirmDialogProvider>
    </ToastProvider>,
  );
}

describe("useCrudPage handleDelete 409 → force flow", () => {
  it("re-prompts with used_by list when delete returns 409 and force-deletes on confirm", async () => {
    // First delete call: 409 with used_by
    const deleteStub = vi
      .spyOn(configApi, "delete")
      .mockRejectedValueOnce(
        new ConfigApiError(409, {
          error: "blocked",
          used_by: [{ namespace: "models", id: "model-a" }],
        }),
      )
      // Force delete call: success
      .mockResolvedValueOnce(undefined);

    vi.spyOn(configApi, "list").mockResolvedValue({
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

    // Second dialog: "Still delete?" with used_by list
    expect(screen.getByText(/Still delete\?/)).toBeTruthy();
    expect(screen.getByText(/models\/model-a/)).toBeTruthy();

    // Confirm force delete
    await act(async () => {
      fireEvent.click(screen.getByText("Force delete"));
    });

    // delete called twice: first without force, then with force
    expect(deleteStub).toHaveBeenCalledTimes(2);
    expect(deleteStub.mock.calls[0]).toEqual(["providers", "prov-a"]);
    expect(deleteStub.mock.calls[1]).toEqual([
      "providers",
      "prov-a",
      { force: true },
    ]);
  });

  it("cancels without force-delete when user dismisses the second dialog", async () => {
    const deleteStub = vi
      .spyOn(configApi, "delete")
      .mockRejectedValueOnce(
        new ConfigApiError(409, {
          error: "blocked",
          used_by: [{ namespace: "models", id: "model-b" }],
        }),
      );

    vi.spyOn(configApi, "list").mockResolvedValue({
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
    vi.spyOn(configApi, "delete").mockRejectedValueOnce(
      new ConfigApiError(500, { error: "internal error" }),
    );

    vi.spyOn(configApi, "list").mockResolvedValue({
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
});
