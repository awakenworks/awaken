import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
import { useState } from "react";
import { cx } from "../internal/cx.js";
import { CopyButton } from "../primitives/copy-button.js";
const DEFAULT_LABELS = {
    summary: "JSON",
    copy: "Copy",
    copied: "Copied",
};
export function JsonInspector({ value, collapsed = false, labels, classes, stringify = (input) => JSON.stringify(input, null, 2) ?? "undefined", }) {
    const [open, setOpen] = useState(!collapsed);
    const text = stringify(value);
    const resolvedLabels = { ...DEFAULT_LABELS, ...labels };
    return (_jsxs("div", { className: cx("ui-json-inspector", classes?.root), children: [_jsxs("div", { className: cx("ui-json-inspector__header", classes?.header), children: [_jsxs("button", { type: "button", "aria-expanded": open, className: cx("ui-json-inspector__toggle", classes?.toggle), onClick: () => setOpen((current) => !current), children: [_jsx("span", { "aria-hidden": "true", children: open ? "▾" : "▸" }), resolvedLabels.summary] }), _jsx(CopyButton, { className: cx("ui-json-inspector__copy", classes?.copy), copiedIcon: resolvedLabels.copied, copiedLabel: resolvedLabels.copied, icon: resolvedLabels.copy, label: resolvedLabels.copy, value: text })] }), open ? _jsx("pre", { className: cx("ui-json-inspector__body", classes?.body), children: text }) : null] }));
}
