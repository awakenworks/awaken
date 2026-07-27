import { type ReactNode } from "react";
export type ChatThinkingProps = {
    readonly label: string;
    readonly formatElapsed?: (seconds: number) => string;
    readonly icon?: ReactNode;
    readonly className?: string;
    readonly classes?: {
        readonly icon?: string;
        readonly label?: string;
        readonly dots?: string;
    };
};
export declare function ChatThinking({ label, formatElapsed, icon, className, classes }: ChatThinkingProps): import("react/jsx-runtime").JSX.Element;
export type ReasoningBlockProps = {
    readonly label: string;
    readonly children: ReactNode;
    readonly defaultOpen?: boolean;
    readonly streaming?: boolean;
    readonly icon?: ReactNode;
    readonly expandIcon?: ReactNode;
    readonly className?: string;
    readonly classes?: {
        readonly header?: string;
        readonly icon?: string;
        readonly chevron?: string;
        readonly body?: string;
        readonly caret?: string;
    };
};
export declare function ReasoningBlock({ label, children, defaultOpen, streaming, icon, expandIcon, className, classes, }: ReasoningBlockProps): import("react/jsx-runtime").JSX.Element;
//# sourceMappingURL=status.d.ts.map