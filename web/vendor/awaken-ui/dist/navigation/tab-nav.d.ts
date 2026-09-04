import { type AnchorHTMLAttributes, type HTMLAttributes, type ReactElement } from "react";
export type TabNavProps = HTMLAttributes<HTMLElement> & {
    readonly label: string;
};
export declare function TabNav({ label, className, children, ...props }: TabNavProps): import("react/jsx-runtime").JSX.Element;
export type TabNavItemProps = AnchorHTMLAttributes<HTMLAnchorElement> & {
    readonly current?: boolean;
    readonly render?: ReactElement<AnchorHTMLAttributes<HTMLAnchorElement>>;
};
export declare function TabNavItem({ current, render, className, children, ...props }: TabNavItemProps): import("react/jsx-runtime").JSX.Element;
