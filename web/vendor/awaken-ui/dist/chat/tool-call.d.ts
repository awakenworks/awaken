import { type ReactNode } from "react";
import type { ToolCallTone, ToolCallView } from "./model.js";
export type ToolCallLabels = {
    readonly input: string;
    readonly output: string;
    readonly inputAriaLabel: string;
    readonly outputAriaLabel: string;
};
export type ToolCallCardProps = Omit<ToolCallView, "id"> & {
    readonly labels: ToolCallLabels;
    readonly icon?: ReactNode;
    readonly expandIcon?: ReactNode;
    readonly badges?: ReactNode;
    readonly defaultOpen?: boolean;
    readonly className?: string;
    readonly classes?: {
        readonly header?: string;
        readonly icon?: string;
        readonly name?: string;
        readonly status?: string;
        readonly chevron?: string;
        readonly body?: string;
        readonly label?: string;
        readonly pre?: string;
        readonly result?: string;
    } | undefined;
};
export declare function ToolCallCard({ name, statusLabel, tone, input, output, labels, icon, expandIcon, badges, defaultOpen, className, classes, }: ToolCallCardProps): import("react/jsx-runtime").JSX.Element;
export type ToolCallGroupProps = {
    readonly calls: ReadonlyArray<ToolCallView>;
    readonly summaryLabel: string;
    readonly labels: ToolCallLabels;
    readonly defaultOpen?: boolean;
    readonly icon?: ReactNode;
    readonly expandIcon?: ReactNode;
    readonly className?: string;
    readonly classes?: {
        readonly header?: string;
        readonly icon?: string;
        readonly summary?: string;
        readonly dot?: string;
        readonly chevron?: string;
        readonly body?: string;
    };
    readonly callClasses?: ToolCallCardProps["classes"] | undefined;
};
export declare function aggregateToolCallTone(calls: ReadonlyArray<ToolCallView>): ToolCallTone;
export declare function ToolCallGroup({ calls, summaryLabel, labels, defaultOpen, icon, expandIcon, className, classes, callClasses }: ToolCallGroupProps): import("react/jsx-runtime").JSX.Element | null;
//# sourceMappingURL=tool-call.d.ts.map