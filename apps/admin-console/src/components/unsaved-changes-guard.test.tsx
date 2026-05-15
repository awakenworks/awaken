// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, waitFor } from "@testing-library/react";
import { useUnsavedChangesGuard } from "./unsaved-changes-guard";

const guardState = vi.hoisted(() => ({
  blocker: {
    state: "unblocked" as "unblocked" | "blocked",
    proceed: vi.fn(),
    reset: vi.fn(),
  },
  predicate: null as null | ((input: { currentLocation: { pathname: string }; nextLocation: { pathname: string } }) => boolean),
  confirmDialog: vi.fn(),
}));

vi.mock("react-router", () => ({
  useBlocker: (predicate: typeof guardState.predicate) => {
    guardState.predicate = predicate;
    return guardState.blocker;
  },
}));

vi.mock("./confirm-dialog", () => ({
  useConfirmDialog: () => guardState.confirmDialog,
}));

function GuardHarness({ enabled = true }: { enabled?: boolean }) {
  useUnsavedChangesGuard({
    enabled,
    title: "Leave page?",
    description: "Discard local edits?",
    confirmLabel: "Discard",
    cancelLabel: "Stay",
  });
  return <div>guard mounted</div>;
}

beforeEach(() => {
  guardState.blocker = {
    state: "unblocked",
    proceed: vi.fn(),
    reset: vi.fn(),
  };
  guardState.predicate = null;
  guardState.confirmDialog = vi.fn(async () => true);
});

afterEach(() => {
  cleanup();
});

describe("useUnsavedChangesGuard", () => {
  it("blocks only cross-path navigation while enabled", () => {
    render(<GuardHarness enabled />);

    expect(guardState.predicate?.({
      currentLocation: { pathname: "/models" },
      nextLocation: { pathname: "/models" },
    })).toBe(false);
    expect(guardState.predicate?.({
      currentLocation: { pathname: "/models" },
      nextLocation: { pathname: "/providers" },
    })).toBe(true);

    cleanup();
    render(<GuardHarness enabled={false} />);
    expect(guardState.predicate?.({
      currentLocation: { pathname: "/models" },
      nextLocation: { pathname: "/providers" },
    })).toBe(false);
  });

  it("proceeds or resets blocked navigation based on the confirm result", async () => {
    guardState.blocker.state = "blocked";
    guardState.confirmDialog = vi.fn(async () => true);
    render(<GuardHarness enabled />);

    await waitFor(() => expect(guardState.blocker.proceed).toHaveBeenCalledTimes(1));
    expect(guardState.confirmDialog).toHaveBeenCalledWith({
      title: "Leave page?",
      description: "Discard local edits?",
      confirmLabel: "Discard",
      cancelLabel: "Stay",
      tone: "destructive",
    });

    cleanup();
    guardState.blocker = { state: "blocked", proceed: vi.fn(), reset: vi.fn() };
    guardState.confirmDialog = vi.fn(async () => false);
    render(<GuardHarness enabled />);

    await waitFor(() => expect(guardState.blocker.reset).toHaveBeenCalledTimes(1));
    expect(guardState.blocker.proceed).not.toHaveBeenCalled();
  });

  it("registers browser beforeunload only while enabled", () => {
    const { rerender } = render(<GuardHarness enabled={false} />);
    const disabledEvent = new Event("beforeunload", { cancelable: true });
    window.dispatchEvent(disabledEvent);
    expect(disabledEvent.defaultPrevented).toBe(false);

    rerender(<GuardHarness enabled />);
    const enabledEvent = new Event("beforeunload", { cancelable: true });
    window.dispatchEvent(enabledEvent);
    expect(enabledEvent.defaultPrevented).toBe(true);
  });
});
