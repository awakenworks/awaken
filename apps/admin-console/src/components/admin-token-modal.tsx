import { useEffect, useRef, useState, type FormEvent } from "react";
import { useFocusTrap } from "./focus-trap";

export interface AdminTokenModalProps {
  open: boolean;
  initialToken: string;
  reason: "manual" | "unauthorized";
  onSubmit: (token: string) => void;
  onClear: () => void;
  onCancel: () => void;
}

export function AdminTokenModal({
  open,
  initialToken,
  reason,
  onSubmit,
  onClear,
  onCancel,
}: AdminTokenModalProps) {
  const [draft, setDraft] = useState(initialToken);
  const inputRef = useRef<HTMLInputElement>(null);
  const backdropRef = useRef<HTMLDivElement>(null);
  const mouseDownOnBackdropRef = useRef(false);

  useFocusTrap(open, backdropRef, { initialFocus: inputRef });

  useEffect(() => {
    if (open) {
      setDraft(initialToken);
      // Select existing text so the user can immediately type a replacement.
      // Focus is moved by useFocusTrap; select() runs after it settles.
      inputRef.current?.select();
    }
  }, [open, initialToken]);

  useEffect(() => {
    if (!open) return;
    function onKey(event: KeyboardEvent) {
      if (event.key === "Escape") {
        onCancel();
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onCancel]);

  if (!open) return null;

  function handleSubmit(event: FormEvent) {
    event.preventDefault();
    onSubmit(draft);
  }

  return (
    <div
      ref={backdropRef}
      role="dialog"
      aria-modal="true"
      aria-labelledby="admin-token-modal-title"
      className="fixed inset-0 z-50 flex items-center justify-center bg-overlay px-4"
      onMouseDown={(event) => {
        mouseDownOnBackdropRef.current = event.target === event.currentTarget;
      }}
      onMouseUp={(event) => {
        if (
          mouseDownOnBackdropRef.current &&
          event.target === event.currentTarget
        ) {
          onCancel();
        }
        mouseDownOnBackdropRef.current = false;
      }}
    >
      <form
        onSubmit={handleSubmit}
        className="w-full max-w-md rounded-sm border border-line bg-surface p-6 shadow-2xl"
      >
        <h2
          id="admin-token-modal-title"
          className="text-lg font-semibold text-fg-strong"
        >
          {reason === "unauthorized" ? "Admin token required" : "Set admin token"}
        </h2>
        <p className="mt-2 text-sm text-fg-soft">
          {reason === "unauthorized"
            ? "The backend rejected the last request. Paste the bearer token to retry."
            : "Tokens are stored in this browser only and sent as Authorization: Bearer."}
        </p>
        <input
          ref={inputRef}
          type="password"
          autoComplete="off"
          value={draft}
          onChange={(event) => setDraft(event.target.value)}
          placeholder="Bearer token"
          className="mt-4 w-full rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
        />
        <div className="mt-5 flex flex-wrap items-center justify-end gap-3">
          <button
            type="button"
            onClick={onClear}
            className="text-sm font-medium text-tone-error transition hover:text-tone-error"
          >
            Clear stored token
          </button>
          <div className="flex-1" />
          <button
            type="button"
            onClick={onCancel}
            className="rounded-sm border border-line-strong px-4 py-2 text-sm font-medium text-fg transition hover:bg-soft"
          >
            Cancel
          </button>
          <button
            type="submit"
            disabled={draft.trim().length === 0}
            className="rounded-sm bg-accent px-4 py-2 text-sm font-medium text-accent-text transition hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-60"
          >
            Save
          </button>
        </div>
      </form>
    </div>
  );
}
