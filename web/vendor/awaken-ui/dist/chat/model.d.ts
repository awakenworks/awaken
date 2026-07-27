import type { ReactNode } from "react";
export type ChatRole = "user" | "assistant" | "system" | "reasoning";
export type ToolCallTone = "pending" | "running" | "done" | "error";
export type ToolCallView = {
    readonly id: string;
    readonly name: string;
    readonly statusLabel: string;
    readonly tone: ToolCallTone;
    readonly input?: string | null;
    readonly output?: string | null;
};
export type ChatMessageView = {
    readonly id: string;
    readonly role: ChatRole;
    readonly authorLabel: string;
    readonly body?: string;
    readonly timestamp?: string;
    readonly media?: ReactNode;
    readonly tools?: ReadonlyArray<ToolCallView>;
    readonly reasoning?: string;
    readonly reasoningStreaming?: boolean;
    readonly accessory?: ReactNode;
};
//# sourceMappingURL=model.d.ts.map