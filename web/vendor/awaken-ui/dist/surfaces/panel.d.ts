import type { HTMLAttributes } from "react";
export type PanelProps = HTMLAttributes<HTMLElement> & {
    readonly accent?: "agent" | "plain";
};
export declare function Panel({ accent, className, ...props }: PanelProps): import("react/jsx-runtime").JSX.Element;
export declare function PanelHeader({ className, ...props }: HTMLAttributes<HTMLDivElement>): import("react/jsx-runtime").JSX.Element;
export declare function PanelBody({ className, ...props }: HTMLAttributes<HTMLDivElement>): import("react/jsx-runtime").JSX.Element;
