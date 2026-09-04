import { type InputHTMLAttributes, type ReactNode, type SelectHTMLAttributes, type TextareaHTMLAttributes } from "react";
export type FieldContext = {
    readonly describedBy: string | undefined;
    readonly id: string;
    readonly invalid: boolean;
};
export type FieldProps = {
    readonly children: (context: FieldContext) => ReactNode;
    readonly label?: ReactNode;
    readonly action?: ReactNode;
    readonly error?: ReactNode;
    readonly help?: ReactNode;
    readonly info?: string | undefined;
    readonly required?: boolean | undefined;
    readonly className?: string | undefined;
    readonly labelClassName?: string | undefined;
    readonly helpClassName?: string | undefined;
    readonly errorClassName?: string | undefined;
    readonly labelAs?: "label" | "div";
    readonly controlId?: string | undefined;
};
export declare function Field({ children, label, action, error, help, info, required, className, labelClassName, helpClassName, errorClassName, labelAs, controlId, }: FieldProps): import("react/jsx-runtime").JSX.Element;
type CommonFieldProps = {
    readonly label?: ReactNode;
    readonly action?: ReactNode;
    readonly error?: ReactNode;
    readonly help?: ReactNode;
    readonly info?: string | undefined;
    readonly fieldClassName?: string | undefined;
    readonly labelClassName?: string | undefined;
    readonly helpClassName?: string | undefined;
};
export declare function TextField({ label, action, error, help, info, fieldClassName, labelClassName, helpClassName, className, id, ...props }: InputHTMLAttributes<HTMLInputElement> & CommonFieldProps): import("react/jsx-runtime").JSX.Element;
export declare function TextAreaField({ label, action, error, help, info, fieldClassName, labelClassName, helpClassName, className, id, ...props }: TextareaHTMLAttributes<HTMLTextAreaElement> & CommonFieldProps): import("react/jsx-runtime").JSX.Element;
export declare function SelectField({ label, action, error, help, info, fieldClassName, labelClassName, helpClassName, className, children, id, ...props }: SelectHTMLAttributes<HTMLSelectElement> & CommonFieldProps): import("react/jsx-runtime").JSX.Element;
export declare function CheckboxField({ label, help, className, ...props }: InputHTMLAttributes<HTMLInputElement> & {
    readonly label: ReactNode;
    readonly help?: ReactNode;
}): import("react/jsx-runtime").JSX.Element;
export {};
