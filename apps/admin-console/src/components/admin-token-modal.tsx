import { useEffect, useRef, useState, type FormEvent } from "react";

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

  useEffect(() => {
    if (open) {
      setDraft(initialToken);
    }
  }, [open, initialToken]);

  useEffect(() => {
    if (open && inputRef.current) {
      inputRef.current.focus();
      inputRef.current.select();
    }
  }, [open]);

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
      role="dialog"
      aria-modal="true"
      aria-labelledby="admin-token-modal-title"
      className="fixed inset-0 z-50 flex items-center justify-center bg-slate-950/40 px-4"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) {
          onCancel();
        }
      }}
    >
      <form
        onSubmit={handleSubmit}
        className="w-full max-w-md rounded-2xl border border-slate-200 bg-white p-6 shadow-2xl"
      >
        <h2
          id="admin-token-modal-title"
          className="text-lg font-semibold text-slate-950"
        >
          {reason === "unauthorized" ? "Admin token required" : "Set admin token"}
        </h2>
        <p className="mt-2 text-sm text-slate-600">
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
          className="mt-4 w-full rounded-xl border border-slate-300 px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
        />
        <div className="mt-5 flex flex-wrap items-center justify-end gap-3">
          <button
            type="button"
            onClick={onClear}
            className="text-sm font-medium text-rose-600 transition hover:text-rose-700"
          >
            Clear stored token
          </button>
          <div className="flex-1" />
          <button
            type="button"
            onClick={onCancel}
            className="rounded-xl border border-slate-300 px-4 py-2 text-sm font-medium text-slate-700 transition hover:bg-slate-50"
          >
            Cancel
          </button>
          <button
            type="submit"
            disabled={draft.trim().length === 0}
            className="rounded-xl bg-slate-950 px-4 py-2 text-sm font-medium text-white transition hover:bg-slate-800 disabled:cursor-not-allowed disabled:opacity-60"
          >
            Save
          </button>
        </div>
      </form>
    </div>
  );
}
