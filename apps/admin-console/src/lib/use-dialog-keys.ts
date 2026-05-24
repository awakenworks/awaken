import { useEffect, useRef } from "react";

/** Minimum-viable dialog keyboard / focus contract.
 *
 *  - Escape closes the dialog (matches WAI-ARIA dialog pattern).
 *  - Focus moves into the dialog on open (first focusable element);
 *    callers can override by setting an explicit `tabindex=-1` target.
 *  - Focus restores to the previously-focused element on close.
 *
 *  Full focus-trap (Tab cycling within the dialog) is intentionally
 *  not implemented here — that needs `inert` polyfill management and
 *  scroll-locking, both of which deserve a dedicated Dialog primitive.
 *  This hook is the floor; it keeps screen-reader / keyboard users
 *  from being stranded outside the dialog when they hit Escape, and
 *  prevents the previous focus from being lost. */
export function useDialogKeys({
  dialogRef,
  onClose,
}: {
  dialogRef: React.RefObject<HTMLElement | null>;
  onClose: () => void;
}) {
  // Persist the close callback in a ref so the effect doesn't restart
  // when the caller passes a new closure each render.
  const onCloseRef = useRef(onClose);
  useEffect(() => {
    onCloseRef.current = onClose;
  }, [onClose]);

  useEffect(() => {
    const previouslyFocused = (document.activeElement as HTMLElement | null) ?? null;
    const dialog = dialogRef.current;
    if (!dialog) return;

    // Initial focus: first focusable element inside the dialog.
    const focusable = dialog.querySelector<HTMLElement>(
      'a[href], button:not([disabled]), textarea:not([disabled]), input:not([disabled]):not([type="hidden"]), select:not([disabled]), [tabindex]:not([tabindex="-1"])',
    );
    focusable?.focus();

    function onKeyDown(e: KeyboardEvent) {
      if (e.key === "Escape") {
        e.stopPropagation();
        onCloseRef.current();
      }
    }
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("keydown", onKeyDown);
      // Restore focus only if the previous element is still mounted.
      if (previouslyFocused && document.contains(previouslyFocused)) {
        try {
          previouslyFocused.focus();
        } catch {
          // Ignore — some elements (detached, removed) throw on focus.
        }
      }
    };
    // dialogRef is a ref; we capture its `.current` at effect start.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
}
