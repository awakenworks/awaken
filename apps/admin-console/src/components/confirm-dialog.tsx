import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { useFocusTrap } from "./focus-trap";

export type ConfirmTone = "neutral" | "destructive";

export interface ConfirmRequest {
  title: string;
  description?: ReactNode;
  confirmLabel?: string;
  cancelLabel?: string;
  tone?: ConfirmTone;
}

type ConfirmFn = (request: ConfirmRequest) => Promise<boolean>;

const ConfirmDialogContext = createContext<ConfirmFn | null>(null);

interface PendingState extends ConfirmRequest {
  resolve: (value: boolean) => void;
}

export function ConfirmDialogProvider({ children }: { children: ReactNode }) {
  const [pending, setPending] = useState<PendingState | null>(null);
  const confirmButtonRef = useRef<HTMLButtonElement>(null);
  const dialogRef = useRef<HTMLDivElement>(null);
  const mouseDownOnBackdropRef = useRef(false);

  useFocusTrap(pending !== null, dialogRef, { initialFocus: confirmButtonRef });

  const confirm = useCallback<ConfirmFn>((request) => {
    return new Promise<boolean>((resolve) => {
      setPending({ ...request, resolve });
    });
  }, []);

  const respond = useCallback(
    (value: boolean) => {
      const current = pending;
      setPending(null);
      current?.resolve(value);
    },
    [pending],
  );

  useEffect(() => {
    if (!pending) return;

    function onKey(event: KeyboardEvent) {
      if (event.key === "Escape") {
        event.preventDefault();
        respond(false);
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [pending, respond]);

  return (
    <ConfirmDialogContext.Provider value={confirm}>
      {children}
      {pending ? (
        <div
          ref={dialogRef}
          role="dialog"
          aria-modal="true"
          aria-labelledby="confirm-dialog-title"
          className="fixed inset-0 z-50 flex items-center justify-center bg-fg-strong/40 px-4"
          onMouseDown={(event) => {
            mouseDownOnBackdropRef.current = event.target === event.currentTarget;
          }}
          onMouseUp={(event) => {
            if (
              mouseDownOnBackdropRef.current &&
              event.target === event.currentTarget
            ) {
              respond(false);
            }
            mouseDownOnBackdropRef.current = false;
          }}
        >
          <div className="w-full max-w-md rounded-2xl border border-line bg-surface p-6 shadow-2xl">
            <h2
              id="confirm-dialog-title"
              className="text-lg font-semibold text-fg-strong"
            >
              {pending.title}
            </h2>
            {pending.description ? (
              <div className="mt-2 text-sm leading-6 text-fg-soft">
                {pending.description}
              </div>
            ) : null}
            <div className="mt-6 flex justify-end gap-3">
              <button
                type="button"
                onClick={() => respond(false)}
                className="rounded-xl border border-line-strong px-4 py-2 text-sm font-medium text-fg transition hover:bg-soft"
              >
                {pending.cancelLabel ?? "Cancel"}
              </button>
              <button
                ref={confirmButtonRef}
                type="button"
                onClick={() => respond(true)}
                className={
                  pending.tone === "destructive"
                    ? "rounded-xl bg-tone-error px-4 py-2 text-sm font-medium text-bg transition hover:bg-tone-error/80"
                    : "rounded-xl bg-fg-strong px-4 py-2 text-sm font-medium text-bg transition hover:bg-fg"
                }
              >
                {pending.confirmLabel ?? "Confirm"}
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </ConfirmDialogContext.Provider>
  );
}

export function useConfirmDialog(): ConfirmFn {
  const ctx = useContext(ConfirmDialogContext);
  if (!ctx) {
    throw new Error(
      "useConfirmDialog must be used inside <ConfirmDialogProvider>",
    );
  }
  return ctx;
}
