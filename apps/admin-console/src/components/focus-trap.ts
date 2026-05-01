import { useEffect, useRef, type RefObject } from "react";

const FOCUSABLE_SELECTORS = [
  "a[href]",
  "button:not([disabled])",
  "input:not([disabled])",
  "select:not([disabled])",
  "textarea:not([disabled])",
  '[tabindex]:not([tabindex="-1"])',
].join(",");

function getFocusableElements(container: HTMLElement): HTMLElement[] {
  return Array.from(container.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTORS));
}

/**
 * Captures focus inside the given container while `active` is true and
 * restores focus to the previously-focused element on deactivation.
 *
 * When `active` becomes true the hook moves focus to `options.initialFocus`
 * if provided, otherwise to the first focusable element inside the container,
 * otherwise to the container itself (tabIndex=-1 is applied if needed).
 */
export function useFocusTrap(
  active: boolean,
  containerRef: RefObject<HTMLElement | null>,
  options?: { initialFocus?: RefObject<HTMLElement | null> },
): void {
  const previousFocusRef = useRef<Element | null>(null);
  // Stable ref identity avoids re-running the effect when the caller passes
  // an inline options object on every render.
  const initialFocusRef = options?.initialFocus ?? null;

  useEffect(() => {
    if (!active) return;

    previousFocusRef.current = document.activeElement;

    const container = containerRef.current;

    // Move initial focus into the trap
    if (initialFocusRef?.current) {
      initialFocusRef.current.focus();
    } else if (container) {
      const focusable = getFocusableElements(container);
      if (focusable.length > 0) {
        focusable[0].focus();
      } else {
        if (!container.hasAttribute("tabindex")) {
          container.setAttribute("tabindex", "-1");
        }
        container.focus();
      }
    }

    function onKeyDown(event: KeyboardEvent) {
      if (event.key !== "Tab") return;
      const trap = containerRef.current;
      if (!trap) return;

      const focusable = getFocusableElements(trap);
      if (focusable.length === 0) return;

      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      const focused = document.activeElement;

      if (event.shiftKey) {
        if (focused === first || !trap.contains(focused)) {
          event.preventDefault();
          last.focus();
        }
      } else {
        if (focused === last || !trap.contains(focused)) {
          event.preventDefault();
          first.focus();
        }
      }
    }

    document.addEventListener("keydown", onKeyDown);

    return () => {
      document.removeEventListener("keydown", onKeyDown);
      const prev = previousFocusRef.current;
      if (prev && typeof (prev as HTMLElement).focus === "function") {
        (prev as HTMLElement).focus();
      }
      previousFocusRef.current = null;
    };
  }, [active, containerRef, initialFocusRef]);
}
