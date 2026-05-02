// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, act, fireEvent } from "@testing-library/react";
import { StrictMode } from "react";
import { ToastProvider, useToast } from "./toast-provider";
import { DEFAULT_DURATIONS_MS, MAX_VISIBLE_TOASTS } from "@/lib/toast-queue";

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

describe("ToastProvider — timer-based auto-dismiss", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  function SuccessApp() {
    const { push } = useToast();
    return (
      <button
        type="button"
        data-testid="push-btn"
        onClick={() => push({ tone: "success", message: "Auto-dismiss me" })}
      >
        Push
      </button>
    );
  }

  it("auto-dismisses a success toast after DEFAULT_DURATIONS_MS.success", () => {
    render(
      <ToastProvider>
        <SuccessApp />
      </ToastProvider>,
    );

    act(() => {
      fireEvent.click(screen.getByTestId("push-btn"));
    });

    expect(screen.getByRole("alert")).toBeTruthy();

    act(() => {
      vi.advanceTimersByTime(DEFAULT_DURATIONS_MS.success + 100);
    });

    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("error toast (durationMs:0) persists after 60 seconds", () => {
    function ErrorApp() {
      const { push } = useToast();
      return (
        <button
          type="button"
          data-testid="push-btn"
          onClick={() => push({ tone: "error", message: "Sticky error", durationMs: 0 })}
        >
          Push
        </button>
      );
    }

    render(
      <ToastProvider>
        <ErrorApp />
      </ToastProvider>,
    );

    act(() => {
      fireEvent.click(screen.getByTestId("push-btn"));
    });

    expect(screen.getByRole("alert")).toBeTruthy();

    act(() => {
      vi.advanceTimersByTime(60_000);
    });

    expect(screen.getByRole("alert")).toBeTruthy();
  });
});

describe("ToastProvider — MAX_VISIBLE_TOASTS cap", () => {
  function MultiPushApp() {
    const { push } = useToast();
    return (
      <button
        type="button"
        data-testid="push-btn"
        onClick={() => {
          for (let i = 1; i <= MAX_VISIBLE_TOASTS + 1; i++) {
            push({ tone: "info", message: `Toast ${i}`, durationMs: 0 });
          }
        }}
      >
        Push all
      </button>
    );
  }

  it("shows only MAX_VISIBLE_TOASTS cards and displaced chip for the excess", () => {
    render(
      <ToastProvider>
        <MultiPushApp />
      </ToastProvider>,
    );

    act(() => {
      fireEvent.click(screen.getByTestId("push-btn"));
    });

    const alerts = screen.getAllByRole("alert");
    expect(alerts).toHaveLength(MAX_VISIBLE_TOASTS);
    expect(screen.getByText(`+ 1 earlier`)).toBeTruthy();
  });
});

describe("ToastProvider — StrictMode double-mount timer safety", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("does not leave stale timers after unmount under StrictMode", () => {
    function SuccessApp() {
      const { push } = useToast();
      return (
        <button
          type="button"
          data-testid="push-btn"
          onClick={() => push({ tone: "success", message: "StrictMode toast" })}
        >
          Push
        </button>
      );
    }

    const { unmount } = render(
      <StrictMode>
        <ToastProvider>
          <SuccessApp />
        </ToastProvider>
      </StrictMode>,
    );

    act(() => {
      fireEvent.click(screen.getByTestId("push-btn"));
    });

    unmount();

    // After unmount the cleanup should have cancelled all timers
    expect(vi.getTimerCount()).toBe(0);

    // Advancing time should not fire any callbacks (no stale timers)
    act(() => {
      vi.advanceTimersByTime(DEFAULT_DURATIONS_MS.success * 2);
    });

    expect(vi.getTimerCount()).toBe(0);
  });
});

describe("ToastProvider — useToast outside provider", () => {
  it("throws when useToast is called outside <ToastProvider>", () => {
    const consoleSpy = vi.spyOn(console, "error").mockImplementation(() => {});

    function Naked() {
      useToast();
      return null;
    }

    expect(() => render(<Naked />)).toThrow(/useToast must be used inside/i);

    consoleSpy.mockRestore();
  });
});
