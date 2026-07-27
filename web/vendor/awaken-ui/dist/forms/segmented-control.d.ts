import type { CSSProperties, ReactNode } from "react";
export type SegmentedControlOption<T> = {
    readonly value: T;
    readonly label: ReactNode;
    readonly ariaLabel?: string;
    readonly disabled?: boolean;
};
export type SegmentedControlProps<T extends string | number | boolean> = {
    readonly options: ReadonlyArray<SegmentedControlOption<T>>;
    readonly value: T;
    readonly onChange: (value: T) => void;
    readonly className?: string;
    readonly buttonClassName?: string;
    readonly activeClassName?: string;
    readonly inactiveClassName?: string;
    readonly buttonStyle?: CSSProperties;
    readonly as?: "div" | "span";
    readonly ariaLabel?: string;
    readonly activeDataAttribute?: `data-${string}`;
};
export declare function SegmentedControl<T extends string | number | boolean>({ options, value, onChange, className, buttonClassName, activeClassName, inactiveClassName, buttonStyle, as: Container, ariaLabel, activeDataAttribute, }: SegmentedControlProps<T>): import("react/jsx-runtime").JSX.Element;
//# sourceMappingURL=segmented-control.d.ts.map