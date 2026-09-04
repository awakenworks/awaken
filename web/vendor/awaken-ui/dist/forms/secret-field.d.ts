import { type ReactNode } from "react";
export type SecretMode = "keep" | "replace" | "clear";
export interface SecretIntent {
    readonly mode: SecretMode;
    readonly value?: string;
}
export interface SecretFieldLabels {
    readonly keep: ReactNode;
    readonly replace: ReactNode;
    readonly clear: ReactNode;
    readonly placeholder: string;
    readonly kept: ReactNode;
    readonly cleared: ReactNode;
}
export interface SecretFieldClasses {
    readonly root?: string;
    readonly label?: string;
    readonly modes?: string;
    readonly modeButton?: string;
    readonly activeMode?: string;
    readonly inactiveMode?: string;
    readonly input?: string;
    readonly status?: string;
}
export interface SecretFieldProps {
    readonly label: ReactNode;
    readonly hasStored: boolean;
    readonly onChange: (intent: SecretIntent) => void;
    readonly labels: SecretFieldLabels;
    readonly placeholder?: string;
    readonly classes?: SecretFieldClasses;
}
/** Write-only keep/replace/clear editor; stored secret values never enter props. */
export declare function SecretField({ label, hasStored, onChange, labels, placeholder, classes, }: SecretFieldProps): import("react/jsx-runtime").JSX.Element;
