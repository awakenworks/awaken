import { type HTMLAttributes, type ReactNode, type RefObject } from "react";
export interface DialogSurfaceProps {
    readonly open: boolean;
    readonly onOpenChange: (open: boolean) => void;
    readonly children: ReactNode;
    readonly rootClassName?: string;
    readonly panelClassName: string;
    readonly overlayClassName?: string;
    readonly panelAs?: "div" | "section";
    readonly panelRef?: RefObject<HTMLElement | null>;
    readonly labelledBy?: string;
    readonly ariaLabel?: string;
    readonly closeOnBackdrop?: boolean;
    readonly panelProps?: Omit<HTMLAttributes<HTMLElement>, "aria-label" | "aria-labelledby" | "children" | "className" | "role">;
}
/**
 * Behavior-only modal boundary for product-specific surfaces. Products own
 * markup classes and tokens; Base UI owns focus, Escape, outside dismissal,
 * portal lifecycle, scroll locking, and focus restoration.
 */
export declare function DialogSurface({ open, onOpenChange, children, rootClassName, panelClassName, overlayClassName, panelAs, panelRef, labelledBy, ariaLabel, closeOnBackdrop, panelProps, }: DialogSurfaceProps): import("react/jsx-runtime").JSX.Element | null;
