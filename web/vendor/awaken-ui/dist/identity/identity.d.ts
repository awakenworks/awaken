import type { HTMLAttributes, ReactNode } from "react";
import { type AvatarProps } from "./avatar.js";
export type IdentityProps = Omit<HTMLAttributes<HTMLDivElement>, "title"> & {
    readonly name: ReactNode;
    readonly description?: ReactNode;
    readonly media?: ReactNode;
    readonly avatar?: AvatarProps;
    readonly status?: ReactNode;
    readonly badges?: ReactNode;
    readonly metadata?: ReactNode;
    readonly actions?: ReactNode;
    readonly classes?: {
        readonly media?: string;
        readonly body?: string;
        readonly heading?: string;
        readonly name?: string;
        readonly description?: string;
        readonly status?: string;
        readonly badges?: string;
        readonly metadata?: string;
        readonly actions?: string;
    };
};
/** Canonical media → name → supporting information identity skeleton. */
export declare function Identity({ name, description, media, avatar, status, badges, metadata, actions, classes, className, ...props }: IdentityProps): import("react/jsx-runtime").JSX.Element;
export type IdentityCardProps = IdentityProps & {
    readonly selected?: boolean;
    readonly href?: string;
    readonly onActivate?: () => void;
};
/** Surface wrapper; routing and domain actions remain consumer-owned. */
export declare function IdentityCard({ selected, href, onActivate, className, ...identity }: IdentityCardProps): import("react/jsx-runtime").JSX.Element;
//# sourceMappingURL=identity.d.ts.map