import type { ReactNode } from "react";
import type { ChatRole } from "./model.js";
export declare function formatChatTime(timestamp: string | undefined): string;
export type ChatMessageProps = {
    readonly role: ChatRole;
    readonly authorLabel: string;
    readonly body?: ReactNode;
    readonly media?: ReactNode;
    readonly timestamp?: string;
    readonly actions?: ReactNode;
    readonly children?: ReactNode;
    readonly compact?: boolean;
    readonly className?: string;
    readonly classes?: {
        readonly media?: string;
        readonly content?: string;
        readonly header?: string;
        readonly author?: string;
        readonly time?: string;
        readonly body?: string;
    };
};
export declare function ChatMessage({ role, authorLabel, body, media, timestamp, actions, children, compact, className, classes, }: ChatMessageProps): import("react/jsx-runtime").JSX.Element;
