import { type KeyboardEvent, type ReactNode, type RefObject } from "react";
export type ChatComposerSendMode = "modifier-enter" | "enter";
export declare function resizeComposerToContent(textarea: HTMLTextAreaElement | null): void;
export declare function useAutoGrowingComposer(value: string): {
    readonly ref: RefObject<HTMLTextAreaElement | null>;
    readonly resize: () => void;
};
export declare function isComposerSubmitShortcut(event: KeyboardEvent<HTMLTextAreaElement>, mode: ChatComposerSendMode): boolean;
export type ChatComposerProps = {
    readonly value: string;
    readonly onChange: (value: string) => void;
    readonly onSubmit: () => void;
    readonly onStop?: () => void;
    readonly busy?: boolean;
    readonly disabled?: boolean;
    readonly sendMode?: ChatComposerSendMode;
    readonly placeholder: string;
    readonly ariaLabel: string;
    readonly sendLabel: string;
    readonly stopLabel?: string;
    readonly hint?: ReactNode;
    readonly leadingActions?: ReactNode;
    readonly sendIcon?: ReactNode;
    readonly stopIcon?: ReactNode;
    readonly className?: string;
    readonly classes?: {
        readonly inputWrapper?: string;
        readonly input?: string;
        readonly controls?: string;
        readonly leading?: string;
        readonly actions?: string;
        readonly send?: string;
        readonly stop?: string;
        readonly hint?: string;
    };
};
export declare function ChatComposer({ value, onChange, onSubmit, onStop, busy, disabled, sendMode, placeholder, ariaLabel, sendLabel, stopLabel, hint, leadingActions, sendIcon, stopIcon, className, classes, }: ChatComposerProps): import("react/jsx-runtime").JSX.Element;
