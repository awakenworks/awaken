import type { HTMLAttributes, ReactNode } from "react";
export type AvatarSize = "sm" | "md" | "lg";
export interface AvatarClasses {
    readonly image?: string;
    readonly fallback?: string;
}
export type AvatarProps = Omit<HTMLAttributes<HTMLSpanElement>, "children"> & {
    readonly label: string;
    readonly size?: AvatarSize;
    readonly src?: string;
    readonly initials?: string;
    readonly children?: ReactNode;
    readonly classes?: AvatarClasses;
};
export declare function initialsOf(label: string): string;
/** Product-neutral identity carrier. Custom generated marks use `children`. */
export declare function Avatar({ label, size, src, initials, children, classes, className, ...props }: AvatarProps): import("react/jsx-runtime").JSX.Element;
export interface AvatarGroupProps extends HTMLAttributes<HTMLSpanElement> {
    readonly children: ReactNode;
    readonly overflow?: number;
    readonly overflowLabel?: (count: number) => string;
    readonly avatarClasses?: AvatarClasses;
}
export declare function AvatarGroup({ children, className, overflow, overflowLabel, avatarClasses, ...props }: AvatarGroupProps): import("react/jsx-runtime").JSX.Element;
