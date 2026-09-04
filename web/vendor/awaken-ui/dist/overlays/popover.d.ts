import { type HTMLAttributes, type ReactElement, type ReactNode } from "react";
export type PopoverRole = "dialog" | "menu" | "listbox";
export type PopoverPlacement = "bottom-end" | "bottom-start";
export interface PopoverProps {
    readonly children: ReactElement;
    readonly content: ReactNode;
    readonly placement?: PopoverPlacement;
    readonly closeOnContentClick?: boolean;
    readonly "aria-label"?: string;
    readonly role?: PopoverRole;
    readonly className?: string;
    readonly contentClassName?: string;
    readonly contentId?: string;
    readonly rootProps?: Omit<HTMLAttributes<HTMLDivElement>, "children" | "className">;
    readonly closeOnMouseLeave?: boolean;
    readonly open?: boolean | undefined;
    readonly defaultOpen?: boolean | undefined;
    readonly onOpenChange?: ((open: boolean) => void) | undefined;
}
export type MenuPopoverProps = Omit<PopoverProps, "role" | "aria-label"> & {
    readonly "aria-label": string;
};
export declare function MenuPopover(props: MenuPopoverProps): import("react/jsx-runtime").JSX.Element;
/**
 * Product-neutral anchored surface. Base UI owns positioning, focus,
 * dismissal, and trigger ARIA; this layer adds the shared menu keyboard and
 * close-after-action contracts used across products.
 */
export declare function Popover({ children, content, placement, closeOnContentClick, "aria-label": ariaLabel, role, className, contentClassName, contentId, rootProps, closeOnMouseLeave, open: controlledOpen, defaultOpen, onOpenChange, }: PopoverProps): import("react/jsx-runtime").JSX.Element;
