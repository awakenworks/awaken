import { type ButtonHTMLAttributes, type ReactNode } from "react";
export type ButtonVariant = "default" | "secondary" | "primary" | "ghost" | "danger" | "icon";
export type ButtonSize = "sm" | "md" | "lg";
export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
    readonly variant?: ButtonVariant;
    readonly size?: ButtonSize;
    readonly icon?: ReactNode;
    readonly loading?: boolean;
    readonly loadingLabel?: string;
}
export declare const Button: import("react").ForwardRefExoticComponent<ButtonProps & import("react").RefAttributes<HTMLButtonElement>>;
//# sourceMappingURL=button.d.ts.map