import { type AnchorHTMLAttributes, type HTMLAttributes, type ReactElement } from "react";
export type BreadcrumbsProps = HTMLAttributes<HTMLElement> & {
    readonly label: string;
};
export declare function Breadcrumbs({ label, className, children, ...props }: BreadcrumbsProps): import("react/jsx-runtime").JSX.Element;
type BreadcrumbItemBaseProps = {
    readonly separator?: ReactElement | string;
};
export type BreadcrumbItemProps = BreadcrumbItemBaseProps & ((HTMLAttributes<HTMLSpanElement> & {
    readonly current: true;
    readonly href?: never;
    readonly render?: never;
}) | (AnchorHTMLAttributes<HTMLAnchorElement> & {
    readonly current?: false;
    readonly render?: ReactElement<AnchorHTMLAttributes<HTMLAnchorElement>>;
}));
export declare function BreadcrumbItem(itemProps: BreadcrumbItemProps): import("react/jsx-runtime").JSX.Element;
export {};
//# sourceMappingURL=breadcrumbs.d.ts.map