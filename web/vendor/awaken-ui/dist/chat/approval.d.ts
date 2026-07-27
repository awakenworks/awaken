import type { ReactNode } from "react";
export type ChatApprovalProps = {
    readonly title: ReactNode;
    readonly description?: ReactNode;
    readonly note?: string;
    readonly onNoteChange?: (note: string) => void;
    readonly noteLabel?: string;
    readonly notePlaceholder?: string;
    readonly approveLabel: string;
    readonly rejectLabel: string;
    readonly onApprove: () => void;
    readonly onReject: () => void;
    readonly pending?: boolean;
};
/** Product-neutral human-in-the-loop decision embedded in a transcript. */
export declare function ChatApproval({ title, description, note, onNoteChange, noteLabel, notePlaceholder, approveLabel, rejectLabel, onApprove, onReject, pending, }: ChatApprovalProps): import("react/jsx-runtime").JSX.Element;
//# sourceMappingURL=approval.d.ts.map