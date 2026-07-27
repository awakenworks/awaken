import { type ReactNode } from "react";
export declare const CHAT_STICK_THRESHOLD = 80;
export declare function isNearChatBottom(scrollTop: number, scrollHeight: number, clientHeight: number, threshold?: number): boolean;
export type ChatMessageListProps = {
    readonly children: ReactNode;
    readonly ariaLabel: string;
    readonly jumpLabel: string;
    readonly busy?: boolean;
    readonly className?: string;
    readonly viewportClassName?: string;
    readonly jumpClassName?: string;
    readonly jumpIcon?: ReactNode;
};
/** Follows streaming output only while the reader remains near the bottom. */
export declare function ChatMessageList({ children, ariaLabel, jumpLabel, busy, className, viewportClassName, jumpClassName, jumpIcon, }: ChatMessageListProps): import("react/jsx-runtime").JSX.Element;
//# sourceMappingURL=message-list.d.ts.map