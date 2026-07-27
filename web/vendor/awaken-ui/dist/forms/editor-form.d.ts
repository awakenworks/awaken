import type { FormEvent, ReactNode } from "react";
export interface EditorFormClasses {
    readonly form?: string;
    readonly actions?: string;
    readonly split?: string;
    readonly formPane?: string;
    readonly assistantPane?: string;
}
export interface EditorFormProps {
    readonly onSubmit: (event: FormEvent<HTMLFormElement>) => void;
    readonly onCancel: () => void;
    readonly error?: ReactNode;
    readonly errorPrefix?: ReactNode;
    readonly pending: boolean;
    readonly cancelLabel: ReactNode;
    readonly submitLabel: ReactNode;
    readonly submitIcon?: ReactNode;
    readonly submitDisabled?: boolean;
    readonly children: ReactNode;
    readonly assistant?: ReactNode;
    readonly classes?: EditorFormClasses;
}
/** Product-neutral form lifecycle/chrome used inside editor dialogs. */
export declare function EditorForm({ onSubmit, onCancel, error, errorPrefix, pending, cancelLabel, submitLabel, submitIcon, submitDisabled, children, assistant, classes, }: EditorFormProps): import("react/jsx-runtime").JSX.Element;
//# sourceMappingURL=editor-form.d.ts.map