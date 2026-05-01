// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, act, screen } from "@testing-library/react";
import { useRef, type ReactNode } from "react";
import { useFocusTrap } from "./focus-trap";

afterEach(() => {
  cleanup();
});

// Basic fixture — no initialFocus option
function TrapFixture({
  active,
  children,
}: {
  active: boolean;
  children?: ReactNode;
}) {
  const ref = useRef<HTMLDivElement>(null);
  useFocusTrap(active, ref);
  return <div ref={ref}>{children}</div>;
}

// Fixture wiring initialFocus to the button with data-testid="initial"
function TrapWithInitialFocus({
  active,
  children,
}: {
  active: boolean;
  children?: ReactNode;
}) {
  const containerRef = useRef<HTMLDivElement>(null);
  const initialRef = useRef<HTMLButtonElement>(null);
  useFocusTrap(active, containerRef, { initialFocus: initialRef });
  return (
    <div ref={containerRef}>
      {children}
      {/* The element that should receive initial focus */}
      <button ref={initialRef} data-testid="initial">
        Initial
      </button>
      <button data-testid="other">Other</button>
    </div>
  );
}

// Fixture with no focusable children and no initialFocus option
function EmptyTrapFixture({ active }: { active: boolean }) {
  const ref = useRef<HTMLDivElement>(null);
  useFocusTrap(active, ref);
  return (
    <div ref={ref} data-testid="container">
      <p>No interactive elements</p>
    </div>
  );
}

function pressTab(shiftKey = false) {
  act(() => {
    document.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "Tab",
        shiftKey,
        bubbles: true,
        cancelable: true,
      }),
    );
  });
}

describe("useFocusTrap", () => {
  it("Tab on last focusable element wraps focus to the first", () => {
    const { container } = render(
      <TrapFixture active>
        <button id="btn-a">A</button>
        <button id="btn-b">B</button>
        <button id="btn-c">C</button>
      </TrapFixture>,
    );

    const buttons = container.querySelectorAll<HTMLButtonElement>("button");
    // Focus the last button
    act(() => buttons[2].focus());
    expect(document.activeElement).toBe(buttons[2]);

    pressTab(false);

    expect(document.activeElement).toBe(buttons[0]);
  });

  it("Shift+Tab on first focusable element wraps focus to the last", () => {
    const { container } = render(
      <TrapFixture active>
        <button id="btn-a">A</button>
        <button id="btn-b">B</button>
        <button id="btn-c">C</button>
      </TrapFixture>,
    );

    const buttons = container.querySelectorAll<HTMLButtonElement>("button");
    act(() => buttons[0].focus());
    expect(document.activeElement).toBe(buttons[0]);

    pressTab(true);

    expect(document.activeElement).toBe(buttons[2]);
  });

  it("restores focus to the previously-focused element when active flips to false", () => {
    // Render a trigger button outside the trap
    const { getByText, rerender } = render(
      <>
        <button id="trigger">Open</button>
        <TrapFixture active={false}>
          <button>Inside</button>
        </TrapFixture>
      </>,
    );

    const trigger = getByText("Open") as HTMLButtonElement;
    act(() => trigger.focus());
    expect(document.activeElement).toBe(trigger);

    // Activate trap — focus moves inside (simulated by component open logic)
    rerender(
      <>
        <button id="trigger">Open</button>
        <TrapFixture active={true}>
          <button>Inside</button>
        </TrapFixture>
      </>,
    );

    // Deactivate — focus should return to trigger
    rerender(
      <>
        <button id="trigger">Open</button>
        <TrapFixture active={false}>
          <button>Inside</button>
        </TrapFixture>
      </>,
    );

    expect(document.activeElement).toBe(trigger);
  });

  it("does not throw when the container has no focusable elements", () => {
    render(
      <TrapFixture active>
        <p>No interactive elements here</p>
      </TrapFixture>,
    );

    // Pressing Tab should silently do nothing
    expect(() => pressTab(false)).not.toThrow();
    expect(() => pressTab(true)).not.toThrow();
  });

  // --- initial focus tests ---

  it("without initialFocus option: focuses the first focusable element on activation", () => {
    const { container } = render(
      <TrapFixture active>
        <button data-testid="first">First</button>
        <button data-testid="second">Second</button>
      </TrapFixture>,
    );

    const first = container.querySelector<HTMLButtonElement>("[data-testid='first']")!;
    expect(document.activeElement).toBe(first);
  });

  it("with initialFocus option: focuses the specified element on activation", () => {
    render(<TrapWithInitialFocus active />);

    const initial = screen.getByTestId("initial");
    expect(document.activeElement).toBe(initial);
  });

  it("container with no focusable elements and no initialFocus: container itself receives focus", () => {
    render(<EmptyTrapFixture active />);

    const container = screen.getByTestId("container");
    expect(document.activeElement).toBe(container);
  });
});
