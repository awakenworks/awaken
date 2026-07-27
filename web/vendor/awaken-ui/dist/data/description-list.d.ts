import type { HTMLAttributes } from "react";
export type DescriptionListProps = HTMLAttributes<HTMLDListElement> & {
    readonly columns?: 1 | 2 | 3;
    readonly density?: "compact" | "default";
};
export declare function DescriptionList({ columns, density, className, ...props }: DescriptionListProps): import("react/jsx-runtime").JSX.Element;
export declare function DescriptionItem({ className, ...props }: HTMLAttributes<HTMLDivElement>): import("react/jsx-runtime").JSX.Element;
export declare function DescriptionTerm({ className, ...props }: HTMLAttributes<HTMLElement>): import("react/jsx-runtime").JSX.Element;
export declare function DescriptionDetails({ className, ...props }: HTMLAttributes<HTMLElement>): import("react/jsx-runtime").JSX.Element;
//# sourceMappingURL=description-list.d.ts.map