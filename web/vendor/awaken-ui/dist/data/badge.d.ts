import type { HTMLAttributes, ReactNode } from "react";
export type UiTone = "neutral" | "success" | "warning" | "danger" | "info" | "agent" | "status" | "priority";
export type BadgeProps = HTMLAttributes<HTMLSpanElement> & {
    readonly tone?: UiTone;
};
export declare function Badge({ children, className, tone, ...props }: BadgeProps): import("react/jsx-runtime").JSX.Element;
export declare function StatusPill({ children, className, tone, ...props }: BadgeProps): import("react/jsx-runtime").JSX.Element;
export type ChipProps = BadgeProps & {
    readonly icon?: ReactNode;
};
export declare function Chip({ children, className, icon, tone, ...props }: ChipProps): import("react/jsx-runtime").JSX.Element;
//# sourceMappingURL=badge.d.ts.map