import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
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

export function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<Toast[]>([]);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const dismiss = useCallback((id: string) => {
    setToasts((current) => dismissToast(current, id));
  }, []);

  const push = useCallback((input: ToastInput): string => {
    const id = nextToastId();
    const toast = createToast(input, id, Date.now());
    setToasts((current) => appendToast(current, toast));
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
      setToasts((current) => expireToasts(current, Date.now()));
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
      <ToastViewport toasts={toasts} onDismiss={dismiss} />
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
  onDismiss,
}: {
  toasts: Toast[];
  onDismiss: (id: string) => void;
}) {
  if (toasts.length === 0) return null;
  return (
    <div
      role="status"
      aria-live="polite"
      className="pointer-events-none fixed bottom-4 right-4 z-50 flex max-w-sm flex-col gap-2"
    >
      {toasts.map((toast) => (
        <ToastCard key={toast.id} toast={toast} onDismiss={onDismiss} />
      ))}
    </div>
  );
}

const TONE_STYLES: Record<
  ToastTone,
  { container: string; badge: string; label: string }
> = {
  success: {
    container: "border-emerald-200 bg-emerald-50 text-emerald-900",
    badge: "bg-emerald-200 text-emerald-900",
    label: "Success",
  },
  error: {
    container: "border-rose-200 bg-rose-50 text-rose-900",
    badge: "bg-rose-200 text-rose-900",
    label: "Error",
  },
  info: {
    container: "border-slate-200 bg-white text-slate-900",
    badge: "bg-slate-200 text-slate-900",
    label: "Info",
  },
};

function ToastCard({
  toast,
  onDismiss,
}: {
  toast: Toast;
  onDismiss: (id: string) => void;
}) {
  const styles = TONE_STYLES[toast.tone];
  return (
    <div
      role="alert"
      data-testid={`toast-${toast.tone}`}
      className={[
        "pointer-events-auto rounded-2xl border p-4 shadow-lg",
        styles.container,
      ].join(" ")}
    >
      <div className="flex items-start gap-3">
        <span
          className={[
            "rounded-full px-2 py-0.5 text-[11px] font-semibold uppercase tracking-[0.18em]",
            styles.badge,
          ].join(" ")}
        >
          {styles.label}
        </span>
        <div className="min-w-0 flex-1">
          <div className="text-sm font-semibold leading-5">{toast.message}</div>
          {toast.detail ? (
            <div className="mt-1 break-words text-xs leading-5 text-slate-700">
              {toast.detail}
            </div>
          ) : null}
        </div>
        <button
          type="button"
          aria-label="Dismiss"
          onClick={() => onDismiss(toast.id)}
          className="rounded-md px-1.5 text-sm text-slate-400 transition hover:text-slate-700"
        >
          ×
        </button>
      </div>
    </div>
  );
}
