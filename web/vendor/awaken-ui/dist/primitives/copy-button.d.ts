import { type ReactNode } from "react";
export type CopyButtonProps = {
    readonly value: string;
    readonly label: string;
    readonly copiedLabel: string;
    readonly icon?: ReactNode;
    readonly copiedIcon?: ReactNode;
    readonly resetAfter?: number;
    readonly className?: string;
};
export declare function CopyButton({ value, label, copiedLabel, icon, copiedIcon, resetAfter, className, }: CopyButtonProps): import("react/jsx-runtime").JSX.Element;
