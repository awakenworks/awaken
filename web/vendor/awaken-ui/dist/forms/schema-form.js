import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
import { useId, useState } from "react";
import { cx } from "../internal/cx.js";
export function stringControlForSchema(schema) {
    return schema.format === "textarea" ? "textarea" : "input";
}
function firstType(schema) {
    return typeof schema.type === "string" ? schema.type : schema.type?.[0];
}
function defaultFor(schema) {
    if (schema.default !== undefined)
        return schema.default;
    switch (firstType(schema)) {
        case "object": return {};
        case "array": return [];
        case "boolean": return false;
        case "number":
        case "integer": return 0;
        case "string": return schema.enum?.[0] ?? "";
        default: return null;
    }
}
function isStructurallyRenderable(schema) {
    if (schema.oneOf || schema.anyOf || schema.allOf || schema.$ref)
        return false;
    return ["object", "string", "number", "integer", "boolean", "array"].includes(firstType(schema) ?? "");
}
function JsonFallback({ value, onChange, controlId, labels, classes }) {
    const [draft, setDraft] = useState(null);
    const [error, setError] = useState("");
    const text = draft ?? JSON.stringify(value ?? null, null, 2);
    return (_jsxs("div", { children: [_jsx("textarea", { id: controlId, className: cx("ui-schema__input", classes?.input, classes?.mono), rows: 4, value: text, onChange: (event) => {
                    setDraft(event.target.value);
                    try {
                        onChange(event.target.value.trim() === "" ? null : JSON.parse(event.target.value));
                        setError("");
                    }
                    catch {
                        setError(labels.invalidJson);
                    }
                } }), error ? _jsx("span", { className: cx("ui-schema__error", classes?.error), children: error }) : null] }));
}
function SchemaNode(props) {
    const { schema, value, onChange, controlId, labels, classes } = props;
    if (!isStructurallyRenderable(schema))
        return _jsx(JsonFallback, { ...props });
    const type = firstType(schema);
    const inputClass = cx("ui-schema__input", classes?.input);
    if (schema.enum) {
        return (_jsx("select", { id: controlId, className: inputClass, value: String(value ?? ""), onChange: (event) => onChange(event.target.value), children: schema.enum.map((option) => (_jsx("option", { value: String(option), children: String(option) }, String(option)))) }));
    }
    if (type === "boolean") {
        return (_jsxs("label", { className: cx("ui-schema__boolean", classes?.row), children: [_jsx("input", { id: controlId, type: "checkbox", checked: Boolean(value), onChange: (event) => onChange(event.target.checked) }), _jsx("span", { className: classes?.muted, children: schema.description ?? "" })] }));
    }
    if (type === "number" || type === "integer") {
        return (_jsx("input", { id: controlId, className: cx(inputClass, classes?.mono), type: "number", value: value === null || value === undefined ? "" : Number(value), onChange: (event) => onChange(event.target.value === "" ? null : Number(event.target.value)) }));
    }
    if (type === "string") {
        return stringControlForSchema(schema) === "textarea" ? (_jsx("textarea", { id: controlId, className: cx(inputClass, classes?.mono), rows: 5, value: String(value ?? ""), onChange: (event) => onChange(event.target.value) })) : (_jsx("input", { id: controlId, className: inputClass, value: String(value ?? ""), onChange: (event) => onChange(event.target.value) }));
    }
    if (type === "array") {
        const items = schema.items ?? {};
        const array = Array.isArray(value) ? value : [];
        return (_jsxs("div", { className: "ui-schema__array", children: [array.map((item, index) => (_jsxs("div", { className: cx("ui-schema__array-row", classes?.row), children: [_jsx("div", { className: "ui-schema__array-value", children: _jsx(SchemaNode, { ...props, schema: items, value: item, controlId: controlId ? `${controlId}-${index}` : undefined, onChange: (next) => onChange(array.map((current, currentIndex) => currentIndex === index ? next : current)) }) }), _jsx("button", { "aria-label": labels.removeItem, className: classes?.button, type: "button", onClick: () => onChange(array.filter((_, currentIndex) => currentIndex !== index)), children: "\u2715" })] }, index))), _jsx("button", { className: classes?.button, type: "button", onClick: () => onChange([...array, defaultFor(items)]), children: labels.addItem })] }));
    }
    const object = value && typeof value === "object" ? value : {};
    const properties = schema.properties ?? {};
    return (_jsx("div", { className: cx("ui-schema__object", classes?.object), children: Object.entries(properties).map(([key, child]) => {
            const childId = controlId ? `${controlId}-${key}` : undefined;
            return (_jsxs("div", { className: cx("ui-schema__field", classes?.field), children: [_jsxs("label", { htmlFor: childId, children: [child.title ?? key, schema.required?.includes(key) ? _jsx("span", { className: "ui-schema__required", children: " *" }) : null] }), child.description && firstType(child) !== "boolean" ? (_jsx("span", { className: classes?.muted, children: child.description })) : null, _jsx(SchemaNode, { ...props, schema: child, value: object[key], controlId: childId, onChange: (next) => onChange({ ...object, [key]: next }) })] }, key));
        }) }));
}
export function SchemaForm({ schema, value, onChange, labels, className, classes, }) {
    const controlId = `schema-${useId().replaceAll(":", "")}`;
    return (_jsx("div", { className: cx("ui-schema", classes?.root, className), children: _jsx(SchemaNode, { schema: schema, value: value, onChange: onChange, controlId: controlId, labels: labels, classes: classes }) }));
}
//# sourceMappingURL=schema-form.js.map