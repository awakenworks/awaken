// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, act, fireEvent } from "@testing-library/react";
import { AdminTokenModal } from "./admin-token-modal";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

function renderModal(overrides: Partial<Parameters<typeof AdminTokenModal>[0]> = {}) {
  const props = {
    open: true,
    initialToken: "",
    reason: "manual" as const,
    onSubmit: vi.fn(),
    onClear: vi.fn(),
    onCancel: vi.fn(),
    ...overrides,
  };
  return { ...render(<AdminTokenModal {...props} />), props };
}

describe("AdminTokenModal", () => {
  it("focuses the token input on open", async () => {
    renderModal();
    const input = await screen.findByPlaceholderText("Bearer token");
    expect(document.activeElement).toBe(input);
  });

  it("Tab cycling stays inside the modal — Tab from last reaches first focusable", () => {
    renderModal({ initialToken: "abc" });

    // Find all buttons and input (focusable elements inside the modal)
    const buttons = screen.getAllByRole("button");
    const lastButton = buttons[buttons.length - 1];

    act(() => lastButton.focus());
    expect(document.activeElement).toBe(lastButton);

    act(() => {
      document.dispatchEvent(
        new KeyboardEvent("keydown", {
          key: "Tab",
          bubbles: true,
          cancelable: true,
        }),
      );
    });

    // Focus should wrap to the first focusable element inside the modal
    const input = screen.getByPlaceholderText("Bearer token");
    expect(document.activeElement).toBe(input);
  });

  it("Tab cycling stays inside — Shift+Tab from input reaches last button", () => {
    renderModal({ initialToken: "abc" });

    const input = screen.getByPlaceholderText("Bearer token");
    act(() => input.focus());
    expect(document.activeElement).toBe(input);

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

    const buttons = screen.getAllByRole("button");
    const lastButton = buttons[buttons.length - 1];
    expect(document.activeElement).toBe(lastButton);
  });

  it("restores focus to the trigger button when the modal closes", () => {
    const trigger = document.createElement("button");
    trigger.textContent = "Open modal";
    document.body.appendChild(trigger);
    act(() => trigger.focus());
    expect(document.activeElement).toBe(trigger);

    const { rerender } = render(
      <AdminTokenModal
        open={true}
        initialToken=""
        reason="manual"
        onSubmit={vi.fn()}
        onClear={vi.fn()}
        onCancel={vi.fn()}
      />,
    );

    // Close the modal
    rerender(
      <AdminTokenModal
        open={false}
        initialToken=""
        reason="manual"
        onSubmit={vi.fn()}
        onClear={vi.fn()}
        onCancel={vi.fn()}
      />,
    );

    expect(document.activeElement).toBe(trigger);
    document.body.removeChild(trigger);
  });

  it("does not call onCancel when mousedown starts on backdrop but mouseup is inside modal", () => {
    const { props } = renderModal();

    const backdrop = screen.getByRole("dialog");

    // Mousedown on backdrop sets the ref flag
    fireEvent.mouseDown(backdrop);

    // Mouseup on a child element (form button) — target is NOT the backdrop
    const cancelBtn = screen.getByRole("button", { name: "Cancel" });
    fireEvent.mouseUp(cancelBtn);

    expect(props.onCancel).not.toHaveBeenCalled();
  });

  it("calls onCancel when both mousedown and mouseup land on the backdrop", () => {
    const { props } = renderModal();

    const backdrop = screen.getByRole("dialog");

    fireEvent.mouseDown(backdrop);
    fireEvent.mouseUp(backdrop);

    expect(props.onCancel).toHaveBeenCalledOnce();
  });
});
