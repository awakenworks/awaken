import { type ReactNode } from "react";
export interface AlertDialogImpact {
    readonly id: string;
    readonly content: ReactNode;
    readonly tone?: "neutral" | "safe" | "danger";
    readonly icon?: ReactNode;
}
export interface AlertDialogClasses {
    readonly backdrop?: string;
    readonly viewport?: string;
    readonly panel?: string;
    readonly header?: string;
    readonly icon?: string;
    readonly title?: string;
    readonly body?: string;
    readonly description?: string;
    readonly impacts?: string;
    readonly impact?: string;
    readonly safeImpact?: string;
    readonly impactIcon?: string;
    readonly footer?: string;
    readonly closeButton?: string;
    readonly cancelButton?: string;
    readonly confirmButton?: string;
}
export interface AlertDialogProps {
    readonly open: boolean;
    readonly onOpenChange: (open: boolean) => void;
    readonly title: ReactNode;
    readonly description?: ReactNode;
    readonly impacts?: readonly AlertDialogImpact[];
    readonly confirmLabel: string;
    readonly cancelLabel: string;
    readonly onConfirm: () => void;
    readonly danger?: boolean;
    readonly icon?: ReactNode;
    readonly closeLabel?: string;
    readonly closeIcon?: ReactNode;
    readonly classes?: AlertDialogClasses;
    readonly role?: "alertdialog" | "dialog";
}
/** Consequence-explicit confirmation dialog with safe initial focus. */
export declare function AlertDialog({ cancelLabel, confirmLabel, danger, description, impacts, onConfirm, onOpenChange, open, title, icon, closeLabel, closeIcon, classes, role, }: AlertDialogProps): import("react/jsx-runtime").JSX.Element;
//# sourceMappingURL=alert-dialog.d.ts.map