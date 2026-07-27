import { type RefObject } from "react";
export declare function hasMarkdown(text: string): boolean;
export declare function renderSafeMarkdown(markdown: string): string;
export type ChatMarkdownProps = {
    readonly body: string;
    readonly copyCodeLabel: string;
    readonly copiedCodeLabel: string;
    readonly copyFailedLabel: string;
    readonly className?: string;
    /**
     * Product-rendered HTML that has already passed the product's sanitizer.
     * This exists for domain link resolvers and diagram placeholders; when
     * omitted, the shared safe Markdown renderer is authoritative.
     */
    readonly sanitizedHtml?: string | null;
    /** Optional product ref for post-render enhancements such as Mermaid. */
    readonly rootRef?: RefObject<HTMLDivElement | null>;
};
export declare function ChatMarkdown({ body, copyCodeLabel, copiedCodeLabel, copyFailedLabel, className, sanitizedHtml, rootRef, }: ChatMarkdownProps): import("react/jsx-runtime").JSX.Element;
//# sourceMappingURL=markdown.d.ts.map