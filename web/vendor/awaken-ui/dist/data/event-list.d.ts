import type { HTMLAttributes, ReactNode, TimeHTMLAttributes } from "react";
export type EventListProps = HTMLAttributes<HTMLOListElement> & {
    readonly density?: "compact" | "default";
};
export declare function EventList({ density, className, ...props }: EventListProps): import("react/jsx-runtime").JSX.Element;
export type EventItemProps = HTMLAttributes<HTMLLIElement> & {
    readonly marker?: ReactNode;
    readonly title: ReactNode;
    readonly timestamp?: ReactNode;
    readonly metadata?: ReactNode;
    readonly actions?: ReactNode;
};
export declare function EventItem({ marker, title, timestamp, metadata, actions, children, className, ...props }: EventItemProps): import("react/jsx-runtime").JSX.Element;
export declare function EventTime({ className, ...props }: TimeHTMLAttributes<HTMLTimeElement>): import("react/jsx-runtime").JSX.Element;
//# sourceMappingURL=event-list.d.ts.map