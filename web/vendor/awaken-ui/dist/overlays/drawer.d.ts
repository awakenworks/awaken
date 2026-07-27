import { type HTMLAttributes, type ReactNode } from "react";
export type DrawerSide = "start" | "end";
export type DrawerSize = "sm" | "md" | "lg";
export type DrawerClasses = {
    readonly backdrop?: string;
    readonly viewport?: string;
    readonly panel?: string;
    readonly header?: string;
    readonly title?: string;
    readonly body?: string;
    readonly footer?: string;
    readonly closeButton?: string;
};
export interface DrawerProps extends Omit<HTMLAttributes<HTMLDivElement>, "title"> {
    readonly open: boolean;
    readonly onOpenChange: (open: boolean) => void;
    readonly title: ReactNode;
    readonly description?: ReactNode;
    readonly children: ReactNode;
    readonly footer?: ReactNode;
    readonly side?: DrawerSide;
    readonly size?: DrawerSize;
    readonly closeLabel: string;
    readonly closeOnOutsidePress?: boolean;
    readonly titleId?: string;
    readonly closeIcon?: ReactNode;
    readonly classes?: DrawerClasses;
}
/** A modal side panel for detail and management surfaces. */
export declare const Drawer: import("react").ForwardRefExoticComponent<DrawerProps & import("react").RefAttributes<HTMLDivElement>>;
//# sourceMappingURL=drawer.d.ts.map