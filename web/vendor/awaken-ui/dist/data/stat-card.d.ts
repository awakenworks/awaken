import { type HTMLAttributes, type ReactElement, type ReactNode } from "react";
export type StatTone = "accent" | "agent" | "danger" | "success" | "warning" | "neutral";
export interface StatCardProps {
    readonly value: ReactNode;
    readonly label: ReactNode;
    readonly icon?: ReactNode;
    readonly hint?: ReactNode;
    readonly tone?: StatTone;
    readonly variant?: "tile" | "metric";
    readonly onClick?: () => void;
    readonly className?: string;
    readonly ariaLabel?: string;
    /** Product navigation adapters may supply a router Link here. */
    readonly render?: ReactElement<HTMLAttributes<HTMLElement>>;
}
export declare function StatCard({ value, label, icon, hint, tone, variant, onClick, className, ariaLabel, render, }: StatCardProps): import("react/jsx-runtime").JSX.Element;
export declare function StatGrid({ className, ...props }: HTMLAttributes<HTMLDivElement>): import("react/jsx-runtime").JSX.Element;
//# sourceMappingURL=stat-card.d.ts.map