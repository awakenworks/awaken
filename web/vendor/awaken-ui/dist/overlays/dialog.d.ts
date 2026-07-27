import { type HTMLAttributes, type ReactNode } from "react";
export type OverlaySize = "sm" | "md" | "lg";
export type DialogClasses = {
    readonly backdrop?: string;
    readonly viewport?: string;
    readonly panel?: string;
    readonly header?: string;
    readonly title?: string;
    readonly body?: string;
    readonly footer?: string;
    readonly closeButton?: string;
};
export interface DialogProps extends Omit<HTMLAttributes<HTMLDivElement>, "title"> {
    readonly open: boolean;
    readonly onOpenChange: (open: boolean) => void;
    readonly title: ReactNode;
    readonly description?: ReactNode;
    readonly children: ReactNode;
    readonly footer?: ReactNode;
    readonly size?: OverlaySize;
    readonly closeLabel: string;
    readonly closeOnOutsidePress?: boolean;
    readonly initialFocus?: boolean;
    readonly titleId?: string;
    readonly closeIcon?: ReactNode;
    readonly classes?: DialogClasses;
}
/**
 * Product-neutral modal dialog. The consumer owns copy and domain actions;
 * this component owns modal focus, dismissal, portal, and ARIA structure.
 */
export declare const Dialog: import("react").ForwardRefExoticComponent<DialogProps & import("react").RefAttributes<HTMLDivElement>>;
//# sourceMappingURL=dialog.d.ts.map