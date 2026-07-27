import { jsx as _jsx, jsxs as _jsxs, Fragment as _Fragment } from "react/jsx-runtime";
import { useId, } from "react";
import { cx } from "../internal/cx.js";
export function Field({ children, label, action, error, help, info, required, className, labelClassName, helpClassName, errorClassName, labelAs = "label", controlId, }) {
    const generatedId = useId();
    const id = controlId ?? generatedId;
    const helpId = help ? `${id}-help` : undefined;
    const errorId = error ? `${id}-error` : undefined;
    const describedBy = [helpId, errorId].filter(Boolean).join(" ") || undefined;
    const labelContent = (_jsxs(_Fragment, { children: [_jsxs("span", { children: [label, info ? _jsx("span", { "aria-label": info, className: "ui-field__info", role: "img", title: info, children: "\u24D8" }) : null] }), action] }));
    return (_jsxs("div", { className: cx("ui-field", error !== undefined && "ui-field--invalid", className), children: [label === undefined ? null : labelAs === "label" ? (_jsx("label", { className: cx("ui-field__label", labelClassName), "data-required": required || undefined, htmlFor: id, children: labelContent })) : _jsx("div", { className: cx("ui-field__label", labelClassName), "data-required": required || undefined, children: labelContent }), children({ describedBy, id, invalid: error !== undefined }), help === undefined ? null : _jsx("span", { className: cx("ui-field__help", helpClassName), id: helpId, children: help }), error === undefined ? null : _jsx("span", { className: cx("ui-field__error", errorClassName), id: errorId, children: error })] }));
}
export function TextField({ label, action, error, help, info, fieldClassName, labelClassName, helpClassName, className, id, ...props }) {
    return _jsx(Field, { className: fieldClassName, labelClassName: labelClassName, helpClassName: helpClassName, label: label, action: action, error: error, help: help, info: info, required: props.required, controlId: id, children: ({ describedBy, id, invalid }) => _jsx("input", { ...props, id: id, className: cx("ui-input", className), "aria-describedby": describedBy, "aria-invalid": invalid }) });
}
export function TextAreaField({ label, action, error, help, info, fieldClassName, labelClassName, helpClassName, className, id, ...props }) {
    return _jsx(Field, { className: fieldClassName, labelClassName: labelClassName, helpClassName: helpClassName, label: label, action: action, error: error, help: help, info: info, required: props.required, controlId: id, children: ({ describedBy, id, invalid }) => _jsx("textarea", { ...props, id: id, className: cx("ui-input", "ui-input--area", className), "aria-describedby": describedBy, "aria-invalid": invalid }) });
}
export function SelectField({ label, action, error, help, info, fieldClassName, labelClassName, helpClassName, className, children, id, ...props }) {
    return _jsx(Field, { className: fieldClassName, labelClassName: labelClassName, helpClassName: helpClassName, label: label, action: action, error: error, help: help, info: info, required: props.required, controlId: id, children: ({ describedBy, id, invalid }) => _jsx("select", { ...props, id: id, className: cx("ui-input", className), "aria-describedby": describedBy, "aria-invalid": invalid, children: children }) });
}
export function CheckboxField({ label, help, className, ...props }) {
    const id = useId();
    const helpId = help ? `${id}-help` : undefined;
    return (_jsxs("div", { className: cx("ui-checkbox", className), children: [_jsxs("label", { className: "ui-checkbox__row", htmlFor: id, children: [_jsx("input", { ...props, type: "checkbox", id: id, "aria-describedby": helpId, className: "ui-checkbox__box" }), _jsx("span", { className: "ui-checkbox__label", children: label })] }), help === undefined ? null : _jsx("span", { className: "ui-field__help", id: helpId, children: help })] }));
}
//# sourceMappingURL=field.js.map