import type { HTMLAttributes, ReactNode } from "react";
export type InlineNoticeTone = "neutral" | "info" | "success" | "warning" | "danger";
export type InlineNoticeProps = Omit<HTMLAttributes<HTMLDivElement>, "title"> & {
    readonly tone?: InlineNoticeTone;
    readonly title?: ReactNode;
    readonly icon?: ReactNode;
    readonly actions?: ReactNode;
    readonly details?: ReactNode;
};
export declare function InlineNotice({ tone, title, icon, actions, details, children, className, ...props }: InlineNoticeProps): import("react/jsx-runtime").JSX.Element;
//# sourceMappingURL=inline-notice.d.ts.map