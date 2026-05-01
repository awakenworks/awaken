// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, act, fireEvent } from "@testing-library/react";
import { ToastProvider, useToast } from "./toast-provider";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

function PushApp({ onParentKeyDown }: { onParentKeyDown?: React.KeyboardEventHandler }) {
  const { push } = useToast();
  return (
    <div onKeyDown={onParentKeyDown}>
      <button
        type="button"
        data-testid="push-btn"
        onClick={() => push({ tone: "info", message: "Hello toast", durationMs: 0 })}
      >
        Push
      </button>
    </div>
  );
}

function renderWithProvider(onParentKeyDown?: React.KeyboardEventHandler) {
  return render(
    <ToastProvider>
      <PushApp onParentKeyDown={onParentKeyDown} />
    </ToastProvider>,
  );
}

describe("ToastProvider — Escape key on dismiss button", () => {
  it("dismisses the toast when Escape is pressed on the dismiss button", () => {
    renderWithProvider();

    act(() => {
      fireEvent.click(screen.getByTestId("push-btn"));
    });

    expect(screen.getByRole("alert")).toBeTruthy();

    const dismissBtn = screen.getByRole("button", { name: "Dismiss" });
    act(() => dismissBtn.focus());
    act(() => {
      fireEvent.keyDown(dismissBtn, { key: "Escape", bubbles: true, cancelable: true });
    });

    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("does not bubble Escape out of the toast viewport to a parent handler", () => {
    const parentHandler = vi.fn();
    renderWithProvider(parentHandler);

    act(() => {
      fireEvent.click(screen.getByTestId("push-btn"));
    });

    expect(screen.getByRole("alert")).toBeTruthy();

    const dismissBtn = screen.getByRole("button", { name: "Dismiss" });
    act(() => dismissBtn.focus());
    act(() => {
      fireEvent.keyDown(dismissBtn, { key: "Escape", bubbles: true, cancelable: true });
    });

    expect(parentHandler).not.toHaveBeenCalled();
  });
});

describe("ToastProvider — displaced count semantics", () => {
  function DisplacedApp() {
    const { push } = useToast();
    let i = 0;
    return (
      <button
        type="button"
        data-testid="push-btn"
        onClick={() => {
          i += 1;
          push({ tone: "info", message: `Toast ${i}`, durationMs: 0 });
        }}
      >
        Push
      </button>
    );
  }

  it("preserves displaced count when a visible toast is dismissed", () => {
    render(
      <ToastProvider>
        <DisplacedApp />
      </ToastProvider>,
    );

    // Push 6 toasts — MAX_VISIBLE_TOASTS is 5, so 1 displaced
    act(() => {
      for (let i = 0; i < 6; i++) {
        fireEvent.click(screen.getByTestId("push-btn"));
      }
    });

    expect(screen.getByText(/\+ 1 earlier/)).toBeTruthy();

    // Dismiss one visible toast — displaced chip should still show
    const dismissBtns = screen.getAllByRole("button", { name: "Dismiss" });
    act(() => {
      fireEvent.click(dismissBtns[0]);
    });

    expect(screen.getByText(/\+ 1 earlier/)).toBeTruthy();
  });

  it("clears displaced only when the chip dismiss button is clicked", () => {
    render(
      <ToastProvider>
        <DisplacedApp />
      </ToastProvider>,
    );

    act(() => {
      for (let i = 0; i < 6; i++) {
        fireEvent.click(screen.getByTestId("push-btn"));
      }
    });

    expect(screen.getByText(/\+ 1 earlier/)).toBeTruthy();

    const chipDismiss = screen.getByRole("button", { name: "Dismiss earlier notifications" });
    act(() => {
      fireEvent.click(chipDismiss);
    });

    expect(screen.queryByText(/earlier/)).toBeNull();
  });
});
