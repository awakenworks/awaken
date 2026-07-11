// Promise-based confirm dialog. useConfirm() returns an async `confirm(opts)` that
// resolves true/false — replaces window.confirm and ad-hoc "are you sure" modals.

import { createContext, useCallback, useContext, useRef, useState, type ReactNode } from "react";
import { useApp } from "../../lib/app-state";
import Modal from "./Modal";

interface ConfirmOpts {
  title: string;
  body?: string;
  confirmLabel?: string;
  cancelLabel?: string;
  danger?: boolean;
}
type ConfirmFn = (opts: ConfirmOpts) => Promise<boolean>;

const Ctx = createContext<ConfirmFn | null>(null);

export function useConfirm(): ConfirmFn {
  const fn = useContext(Ctx);
  if (!fn) throw new Error("useConfirm must be used within <ConfirmProvider>");
  return fn;
}

export function ConfirmProvider({ children }: { children: ReactNode }) {
  const app = useApp();
  const [opts, setOpts] = useState<ConfirmOpts | null>(null);
  const resolver = useRef<((v: boolean) => void) | null>(null);

  const confirm = useCallback<ConfirmFn>((o) => {
    setOpts(o);
    return new Promise<boolean>((resolve) => {
      resolver.current = resolve;
    });
  }, []);

  const close = (v: boolean) => {
    resolver.current?.(v);
    resolver.current = null;
    setOpts(null);
  };

  return (
    <Ctx.Provider value={confirm}>
      {children}
      {opts && (
        <Modal
          title={opts.title}
          onClose={() => close(false)}
          width="min(420px, 92vw)"
          footer={
            <>
              <button className="btn" onClick={() => close(false)}>
                {opts.cancelLabel ?? app.t("Cancel", "取消")}
              </button>
              <button
                className={`btn ${opts.danger ? "danger" : "primary"}`}
                onClick={() => close(true)}
                autoFocus
              >
                {opts.confirmLabel ?? app.t("Confirm", "确认")}
              </button>
            </>
          }
        >
          {opts.body && (
            <p className="mut" style={{ margin: 0 }}>
              {opts.body}
            </p>
          )}
        </Modal>
      )}
    </Ctx.Provider>
  );
}
