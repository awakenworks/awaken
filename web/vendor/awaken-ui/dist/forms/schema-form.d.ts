export interface JsonSchema {
    readonly type?: string | readonly string[];
    readonly title?: string;
    readonly description?: string;
    readonly properties?: Readonly<Record<string, JsonSchema>>;
    readonly required?: readonly string[];
    readonly items?: JsonSchema;
    readonly enum?: readonly unknown[];
    readonly default?: unknown;
    readonly [key: string]: unknown;
}
export interface SchemaFormLabels {
    readonly invalidJson: string;
    readonly addItem: string;
    readonly removeItem: string;
}
export interface SchemaFormClasses {
    readonly root?: string;
    readonly object?: string;
    readonly field?: string;
    readonly input?: string;
    readonly mono?: string;
    readonly row?: string;
    readonly muted?: string;
    readonly error?: string;
    readonly button?: string;
}
export interface SchemaFormProps {
    readonly schema: JsonSchema;
    readonly value: unknown;
    readonly onChange: (value: unknown) => void;
    readonly labels: SchemaFormLabels;
    readonly className?: string;
    readonly classes?: SchemaFormClasses;
}
export declare function stringControlForSchema(schema: JsonSchema): "input" | "textarea";
export declare function SchemaForm({ schema, value, onChange, labels, className, classes, }: SchemaFormProps): import("react/jsx-runtime").JSX.Element;
//# sourceMappingURL=schema-form.d.ts.map