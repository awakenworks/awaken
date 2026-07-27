export type JsonInspectorLabels = {
    readonly summary: string;
    readonly copy: string;
    readonly copied: string;
};
export type JsonInspectorClasses = {
    readonly root?: string | undefined;
    readonly header?: string | undefined;
    readonly toggle?: string | undefined;
    readonly copy?: string | undefined;
    readonly body?: string | undefined;
};
export type JsonInspectorProps = {
    readonly value: unknown;
    readonly collapsed?: boolean | undefined;
    readonly labels?: Partial<JsonInspectorLabels> | undefined;
    readonly classes?: JsonInspectorClasses | undefined;
    readonly stringify?: ((value: unknown) => string) | undefined;
};
export declare function JsonInspector({ value, collapsed, labels, classes, stringify, }: JsonInspectorProps): import("react/jsx-runtime").JSX.Element;
//# sourceMappingURL=json-inspector.d.ts.map