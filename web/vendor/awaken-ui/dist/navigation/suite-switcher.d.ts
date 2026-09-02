import type { ReactElement, ReactNode } from "react";
import { type PopoverPlacement } from "../overlays/popover.js";
export interface SuiteSwitcherProduct {
    readonly id: string;
    readonly label: ReactNode;
    readonly description?: ReactNode;
    readonly href?: string;
    readonly icon?: ReactNode;
    readonly isCurrent?: boolean;
}
export interface SuiteSwitcherDestination {
    readonly id: string;
    readonly label: ReactNode;
    readonly description?: ReactNode;
    readonly href: string;
    readonly icon?: ReactNode;
}
export interface SuiteSwitcherProps {
    readonly "aria-label": string;
    readonly trigger: ReactElement;
    readonly currentLabel: ReactNode;
    readonly products: readonly SuiteSwitcherProduct[];
    readonly destinations?: readonly SuiteSwitcherDestination[];
    readonly placement?: PopoverPlacement;
}
/**
 * Product-neutral suite navigation presentation.
 *
 * Consumers own every label, icon, URL and authorization decision. The shared
 * component owns the accessible menu, current-item treatment, focus/keyboard
 * behavior and the one consistent product/destination layout.
 */
export declare function SuiteSwitcher({ "aria-label": ariaLabel, currentLabel, destinations, placement, products, trigger, }: SuiteSwitcherProps): import("react/jsx-runtime").JSX.Element;
//# sourceMappingURL=suite-switcher.d.ts.map
