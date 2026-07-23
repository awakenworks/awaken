import {
  SchemaForm as SharedSchemaForm,
  stringControlForSchema,
  type JsonSchema,
  type SchemaFormProps as SharedSchemaFormProps,
} from "@awaken/ui";

export type { JsonSchema };
export { stringControlForSchema };

export type SchemaFormProps = Omit<SharedSchemaFormProps, "classes" | "labels">;

/** Awaken visual/copy adapter over the shared JSON Schema renderer. */
export function SchemaForm(props: SchemaFormProps) {
  return (
    <SharedSchemaForm
      {...props}
      classes={{
        button: "btn ghost",
        error: "err",
        field: "field",
        input: "input",
        mono: "mono",
        muted: "mut",
        object: "schema-object",
        root: "schema-form",
        row: "row",
      }}
      labels={{
        addItem: "+ add",
        invalidJson: "invalid JSON — not saved",
        removeItem: "Remove item",
      }}
    />
  );
}
