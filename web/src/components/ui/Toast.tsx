import {
  ToastProvider as SharedToastProvider,
  useToast as useSharedToast,
} from "@awaken/ui";
import { createContext, useContext, useMemo, type ReactNode } from "react";
import { useApp } from "../../lib/app-state";

export type ToastTone = "ok" | "err" | "info";
export type ToastApi = {
  readonly push: (tone: ToastTone, text: string) => void;
  readonly ok: (text: string) => void;
  readonly err: (text: string) => void;
  readonly info: (text: string) => void;
};

const Context = createContext<ToastApi | null>(null);

export function useToast(): ToastApi {
  const api = useContext(Context);
  if (!api) throw new Error("useToast must be used within <ToastProvider>");
  return api;
}

function Bridge({ children }: { readonly children: ReactNode }) {
  const shared = useSharedToast();
  const api = useMemo<ToastApi>(() => {
    const push = (tone: ToastTone, text: string) => {
      shared.push({
        message: text,
        tone: tone === "ok" ? "success" : tone === "err" ? "error" : "info",
        duration: tone === "err" ? 6_000 : 3_200,
      });
    };
    return {
      push,
      ok: (text) => push("ok", text),
      err: (text) => push("err", text),
      info: (text) => push("info", text),
    };
  }, [shared]);
  return <Context.Provider value={api}>{children}</Context.Provider>;
}

export function ToastProvider({ children }: { readonly children: ReactNode }) {
  const app = useApp();
  return (
    <SharedToastProvider
      dismissLabel={app.t("Dismiss notification", "关闭通知")}
      regionLabel={app.t("Notifications", "通知")}
    >
      <Bridge>{children}</Bridge>
    </SharedToastProvider>
  );
}
