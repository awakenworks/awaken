import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useReducer,
  useRef,
  type KeyboardEvent,
  type ReactNode,
} from "react";
import {
  appendToast,
  createToast,
  dismissToast,
  expireToasts,
  nextExpiryDelay,
  type Toast,
  type ToastInput,
  type ToastTone,
} from "@/lib/toast-queue";

interface ToastContextValue {
  push: (input: ToastInput) => string;
  dismiss: (id: string) => void;
  success: (message: string, detail?: string) => string;
  error: (message: string, detail?: string) => string;
  info: (message: string, detail?: string) => string;
}

const ToastContext = createContext<ToastContextValue | null>(null);

let counter = 0;
function nextToastId(): string {
  counter += 1;
  return `toast-${counter}-${Date.now().toString(36)}`;
}

// ---------------------------------------------------------------------------
// Reducer — pure state machine; StrictMode-safe (no side-effects in updater)
// ---------------------------------------------------------------------------

interface ToastState {
  toasts: Toast[];
  displaced: number;
}

type ToastAction =
  | { kind: "push"; toast: Toast }
  | { kind: "dismiss"; id: string }
  | { kind: "clearDisplaced" }
  | { kind: "expire"; now: number };

function toastReducer(state: ToastState, action: ToastAction): ToastState {
  switch (action.kind) {
    case "push": {
      const result = appendToast(state.toasts, action.toast, state.displaced);
      return { toasts: result.queue, displaced: result.displaced };
    }
    case "dismiss": {
      // Leave displaced unchanged — those earlier toasts are not retrievable.
      return { ...state, toasts: dismissToast(state.toasts, action.id) };
    }
    case "clearDisplaced": {
      return { ...state, displaced: 0 };
    }
    case "expire": {
      return { ...state, toasts: expireToasts(state.toasts, action.now) };
    }
  }
}

const INITIAL_STATE: ToastState = { toasts: [], displaced: 0 };

export function ToastProvider({ children }: { children: ReactNode }) {
  const [{ toasts, displaced }, dispatch] = useReducer(toastReducer, INITIAL_STATE);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const dismiss = useCallback((id: string) => {
    dispatch({ kind: "dismiss", id });
  }, []);

  const clearDisplaced = useCallback(() => {
    dispatch({ kind: "clearDisplaced" });
  }, []);

  const push = useCallback((input: ToastInput): string => {
    const id = nextToastId();
    const toast = createToast(input, id, Date.now());
    dispatch({ kind: "push", toast });
    return id;
  }, []);

  const success = useCallback(
    (message: string, detail?: string) => push({ tone: "success", message, detail }),
    [push],
  );
  const error = useCallback(
    (message: string, detail?: string) =>
      push({ tone: "error", message, detail, durationMs: 0 }),
    [push],
  );
  const info = useCallback(
    (message: string, detail?: string) => push({ tone: "info", message, detail }),
    [push],
  );

  useEffect(() => {
    if (timerRef.current !== null) {
      clearTimeout(timerRef.current);
      timerRef.current = null;
    }

    const delay = nextExpiryDelay(toasts, Date.now());
    if (delay === null) {
      return;
    }

    timerRef.current = setTimeout(() => {
      dispatch({ kind: "expire", now: Date.now() });
    }, Math.max(delay, 50));

    return () => {
      if (timerRef.current !== null) {
        clearTimeout(timerRef.current);
        timerRef.current = null;
      }
    };
  }, [toasts]);

  const value = useMemo<ToastContextValue>(
    () => ({ push, dismiss, success, error, info }),
    [push, dismiss, success, error, info],
  );

  return (
    <ToastContext.Provider value={value}>
      {children}
      <ToastViewport toasts={toasts} displaced={displaced} onDismiss={dismiss} onClearDisplaced={clearDisplaced} />
    </ToastContext.Provider>
  );
}

export function useToast(): ToastContextValue {
  const ctx = useContext(ToastContext);
  if (!ctx) {
    throw new Error("useToast must be used inside <ToastProvider>");
  }
  return ctx;
}

function ToastViewport({
  toasts,
  displaced,
  onDismiss,
  onClearDisplaced,
}: {
  toasts: Toast[];
  displaced: number;
  onDismiss: (id: string) => void;
  onClearDisplaced: () => void;
}) {
  if (toasts.length === 0 && displaced === 0) return null;
  return (
    <div
      role="status"
      aria-live="polite"
      className="pointer-events-none fixed bottom-4 right-4 z-50 flex max-w-sm flex-col gap-2"
    >
      {displaced > 0 && (
        <div className="pointer-events-auto flex items-center justify-between rounded-sm border border-line bg-surface px-3 py-1.5 text-xs text-fg-soft shadow">
          <span>+ {displaced} earlier</span>
          <button
            type="button"
            aria-label="Dismiss earlier notifications"
            onClick={onClearDisplaced}
            className="ml-3 rounded px-1 text-fg-faint transition hover:text-fg"
          >
            ×
          </button>
        </div>
      )}
      {toasts.map((toast) => (
        <ToastCard key={toast.id} toast={toast} onDismiss={onDismiss} />
      ))}
    </div>
  );
}

/* Spec (awaken-ui.html .toast): neutral elevated surface + 6px dot LEFT
 * coloured per tone, no UPPERCASE badge, no tinted bg. Status reads via
 * the dot + (sr-only) label rather than a chip. */
const TONE_DOT: Record<ToastTone, string> = {
  success: "bg-tone-success",
  error:   "bg-tone-error",
  info:    "bg-tone-info",
};

const TONE_LABEL: Record<ToastTone, string> = {
  success: "Success",
  error:   "Error",
  info:    "Info",
};

function ToastCard({
  toast,
  onDismiss,
}: {
  toast: Toast;
  onDismiss: (id: string) => void;
}) {
  return (
    <div
      role="alert"
      data-testid={`toast-${toast.tone}`}
      className="pointer-events-auto flex items-start gap-2.5 rounded-sm border border-line-strong bg-surface px-3.5 py-2.5 text-fg shadow-card"
    >
      <span
        aria-hidden
        className={`mt-1.5 inline-block size-1.5 shrink-0 rounded-full ${TONE_DOT[toast.tone]}`}
      />
      <span className="sr-only">{TONE_LABEL[toast.tone]}:</span>
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium leading-5">{toast.message}</div>
        {toast.detail ? (
          <div className="mt-1 break-words text-xs leading-5 text-fg-soft">
            {toast.detail}
          </div>
        ) : null}
      </div>
      <button
        type="button"
        aria-label="Dismiss"
        onClick={() => onDismiss(toast.id)}
        onKeyDown={(e: KeyboardEvent<HTMLButtonElement>) => {
          if (e.key === "Escape") {
            e.preventDefault();
            e.stopPropagation();
            onDismiss(toast.id);
          }
        }}
        className="-mr-1 rounded-sm px-1.5 text-sm text-fg-faint transition hover:text-fg"
      >
        ×
      </button>
    </div>
  );
}
