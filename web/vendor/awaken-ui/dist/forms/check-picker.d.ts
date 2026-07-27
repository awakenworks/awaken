import type { ReactNode } from "react";
export interface CheckPickerOption {
    readonly id: string;
    readonly label?: ReactNode;
    readonly description?: ReactNode;
    readonly disabled?: boolean;
}
export interface CheckPickerClasses {
    readonly root?: string;
    readonly row?: string;
    readonly label?: string;
    readonly description?: string;
    readonly empty?: string;
}
export interface CheckPickerProps {
    readonly options: readonly CheckPickerOption[];
    readonly selected: readonly string[];
    readonly onChange: (next: string[]) => void;
    readonly empty?: ReactNode;
    readonly className?: string;
    readonly classes?: CheckPickerClasses;
}
/** Controlled multi-select list; option order is preserved in emitted values. */
export declare function CheckPicker({ options, selected, onChange, empty, className, classes, }: CheckPickerProps): import("react/jsx-runtime").JSX.Element;
//# sourceMappingURL=check-picker.d.ts.map