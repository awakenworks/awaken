// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, act, fireEvent } from "@testing-library/react";
import { ConfirmDialogProvider, useConfirmDialog } from "./confirm-dialog";
import { useEffect } from "react";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

// Helper component that triggers a confirm dialog on mount
function Trigger({
  onResult,
}: {
  onResult?: (value: boolean) => void;
}) {
  const confirm = useConfirmDialog();

  useEffect(() => {
    confirm({ title: "Delete item?", confirmLabel: "Delete", tone: "destructive" }).then(
      onResult ?? (() => {}),
    );
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return <button id="trigger">Trigger</button>;
}

function renderDialog(onResult?: (value: boolean) => void) {
  return render(
    <ConfirmDialogProvider>
      <Trigger onResult={onResult} />
    </ConfirmDialogProvider>,
  );
}

describe("ConfirmDialog", () => {
  it("focuses the confirm button when the dialog opens", async () => {
    renderDialog();
    const confirmBtn = await screen.findByRole("button", { name: "Delete" });
    expect(document.activeElement).toBe(confirmBtn);
  });

  it("Tab cycling stays inside — Tab from confirm button reaches cancel button", async () => {
    renderDialog();
    const confirmBtn = await screen.findByRole("button", { name: "Delete" });
    act(() => confirmBtn.focus());

    act(() => {
      document.dispatchEvent(
        new KeyboardEvent("keydown", {
          key: "Tab",
          bubbles: true,
          cancelable: true,
        }),
      );
    });

    const cancelBtn = screen.getByRole("button", { name: "Cancel" });
    expect(document.activeElement).toBe(cancelBtn);
  });

  it("Shift+Tab from cancel button wraps to confirm button", async () => {
    renderDialog();
    const cancelBtn = await screen.findByRole("button", { name: "Cancel" });
    act(() => cancelBtn.focus());

    act(() => {
      document.dispatchEvent(
        new KeyboardEvent("keydown", {
          key: "Tab",
          shiftKey: true,
          bubbles: true,
          cancelable: true,
        }),
      );
    });

    const confirmBtn = screen.getByRole("button", { name: "Delete" });
    expect(document.activeElement).toBe(confirmBtn);
  });

  it("restores focus to the trigger button after the dialog closes", async () => {
    const trigger = document.createElement("button");
    trigger.id = "outside-trigger";
    document.body.appendChild(trigger);
    act(() => trigger.focus());

    const onResult = vi.fn();
    renderDialog(onResult);

    const confirmBtn = await screen.findByRole("button", { name: "Delete" });
    act(() => confirmBtn.click());

    // Wait for the promise to resolve and state to settle
    await act(async () => {
      await Promise.resolve();
    });

    expect(document.activeElement).toBe(trigger);
    document.body.removeChild(trigger);
  });

  it("does not call respond when mousedown starts on backdrop but releases inside modal", async () => {
    const onResult = vi.fn();
    renderDialog(onResult);

    const dialog = await screen.findByRole("dialog");
    const inner = dialog.querySelector("div")!;

    // Mousedown on backdrop, mouseup inside inner panel
    fireEvent.mouseDown(dialog, { target: dialog });
    fireEvent.mouseUp(inner, { target: inner });

    // Dialog should still be visible
    expect(screen.queryByRole("dialog")).not.toBeNull();
  });

  it("closes the dialog when both mousedown and mouseup land on the backdrop", async () => {
    renderDialog();

    const dialog = await screen.findByRole("dialog");
    fireEvent.mouseDown(dialog, { target: dialog });
    fireEvent.mouseUp(dialog, { target: dialog });

    await act(async () => {
      await Promise.resolve();
    });

    expect(screen.queryByRole("dialog")).toBeNull();
  });
});
