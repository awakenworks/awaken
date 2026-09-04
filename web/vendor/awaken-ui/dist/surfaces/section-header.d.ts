import type { ReactNode } from "react";
export type SectionHeaderProps = {
    readonly title: ReactNode;
    readonly icon?: ReactNode;
    readonly count?: ReactNode;
    readonly actions?: ReactNode;
    readonly as?: "h2" | "h3" | "h4";
    readonly className?: string;
};
export declare function SectionHeader({ title, icon, count, actions, as: Heading, className }: SectionHeaderProps): import("react/jsx-runtime").JSX.Element;
