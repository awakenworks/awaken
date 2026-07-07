// Toast queue: transient status messages (save ok, publish ok, error). A provider
// holds the queue and renders a bottom-right stack; useToast() pushes into it.

import { createContext, useCallback, useContext, useRef, useState, type ReactNode } from "react";

type ToastTone = "ok" | "err" | "info";
interface Toast {
  id: number;
  tone: ToastTone;
  text: string;
}
interface ToastApi {
  push: (tone: ToastTone, text: string) => void;
  ok: (text: string) => void;
  err: (text: string) => void;
  info: (text: string) => void;
}

const Ctx = createContext<ToastApi | null>(null);

export function useToast(): ToastApi {
  const api = useContext(Ctx);
  if (!api) throw new Error("useToast must be used within <ToastProvider>");
  return api;
}

export function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<Toast[]>([]);
  const seq = useRef(0);
  const dismiss = useCallback((id: number) => setToasts((ts) => ts.filter((t) => t.id !== id)), []);
  const push = useCallback(
    (tone: ToastTone, text: string) => {
      const id = (seq.current += 1);
      setToasts((ts) => [...ts, { id, tone, text }]);
      setTimeout(() => dismiss(id), tone === "err" ? 6000 : 3200);
    },
    [dismiss],
  );
  const api: ToastApi = {
    push,
    ok: (t) => push("ok", t),
    err: (t) => push("err", t),
    info: (t) => push("info", t),
  };
  return (
    <Ctx.Provider value={api}>
      {children}
      <div className="toast-stack" role="status" aria-live="polite">
        {toasts.map((t) => (
          <div key={t.id} className={`toast ${t.tone}`} onClick={() => dismiss(t.id)}>
            <span className="toast-dot" />
            <span>{t.text}</span>
          </div>
        ))}
      </div>
    </Ctx.Provider>
  );
}
