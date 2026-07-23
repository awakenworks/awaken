import {
  ConfirmProvider as SharedConfirmProvider,
  useConfirm as useSharedConfirm,
} from "@awaken/ui";
import { useCallback, type ReactNode } from "react";
import { useApp } from "../../lib/app-state";

export interface ConfirmOpts {
  readonly title: string;
  readonly body?: string;
  readonly confirmLabel?: string;
  readonly cancelLabel?: string;
  readonly danger?: boolean;
}

export type ConfirmFn = (options: ConfirmOpts) => Promise<boolean>;

/** Adds Awaken's bilingual defaults to the shared serialized confirm queue. */
export function useConfirm(): ConfirmFn {
  const confirm = useSharedConfirm();
  const app = useApp();
  return useCallback((options: ConfirmOpts) => confirm({
    title: options.title,
    description: options.body,
    confirmLabel: options.confirmLabel ?? app.t("Confirm", "确认"),
    cancelLabel: options.cancelLabel ?? app.t("Cancel", "取消"),
    danger: options.danger,
  }), [app, confirm]);
}

export function ConfirmProvider({ children }: { readonly children: ReactNode }) {
  return <SharedConfirmProvider>{children}</SharedConfirmProvider>;
}
