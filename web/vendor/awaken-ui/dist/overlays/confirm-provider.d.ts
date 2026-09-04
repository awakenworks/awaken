import { type ReactNode } from "react";
import { type AlertDialogImpact } from "./alert-dialog.js";
export interface ConfirmRequest {
    readonly title: ReactNode;
    readonly description?: ReactNode;
    readonly impacts?: readonly AlertDialogImpact[];
    readonly confirmLabel: string;
    readonly cancelLabel: string;
    readonly danger?: boolean;
}
export type Confirm = (request: ConfirmRequest) => Promise<boolean>;
export declare function useConfirm(): Confirm;
/**
 * Serializes confirmations so concurrent callers cannot replace or orphan an
 * unresolved request. Unmounting settles every pending request as cancelled.
 */
export declare function ConfirmProvider({ children }: {
    readonly children: ReactNode;
}): import("react/jsx-runtime").JSX.Element;
