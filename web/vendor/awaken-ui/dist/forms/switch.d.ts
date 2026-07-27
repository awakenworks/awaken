import type { InputHTMLAttributes } from "react";
export type SwitchProps = Omit<InputHTMLAttributes<HTMLInputElement>, "type"> & {
    readonly label?: string | undefined;
    readonly onCheckedChange?: ((checked: boolean) => void) | undefined;
};
export declare function Switch({ className, label, onChange, onCheckedChange, ...props }: SwitchProps): import("react/jsx-runtime").JSX.Element;
//# sourceMappingURL=switch.d.ts.map