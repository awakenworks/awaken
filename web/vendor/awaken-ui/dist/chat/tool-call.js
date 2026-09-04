import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
import { useState } from "react";
import { cx } from "../internal/cx.js";
export function ToolCallCard({ name, statusLabel, tone, input, output, labels, icon, expandIcon, badges, defaultOpen = false, className, classes, }) {
    const hasDetail = Boolean(input || output);
    const [open, setOpen] = useState(defaultOpen && hasDetail);
    return (_jsxs("div", { className: cx("ui-chat-tool", className), "data-tone": tone, children: [_jsxs("button", { className: cx("ui-chat-tool__header", classes?.header), type: "button", disabled: !hasDetail, "aria-expanded": hasDetail ? open : undefined, onClick: () => setOpen((value) => !value), children: [_jsx("span", { className: cx("ui-chat-tool__icon", classes?.icon), "aria-hidden": "true", children: icon ?? toneMark(tone) }), _jsx("span", { className: cx("ui-chat-tool__name", classes?.name), children: name }), badges, _jsx("span", { className: cx("ui-chat-tool__status", classes?.status), children: statusLabel }), hasDetail ? _jsx("span", { className: cx(classes?.chevron, open && "is-open"), "data-open": open || undefined, "aria-hidden": "true", children: expandIcon ?? (open ? "⌄" : "›") }) : null] }), open ? (_jsxs("div", { className: cx("ui-chat-tool__body", classes?.body), children: [input ? _jsx(ToolDetail, { label: labels.input, ariaLabel: labels.inputAriaLabel, value: input, labelClassName: classes?.label, preClassName: classes?.pre }) : null, output ? _jsx(ToolDetail, { label: labels.output, ariaLabel: labels.outputAriaLabel, value: output, labelClassName: classes?.label, preClassName: cx(classes?.pre, classes?.result) }) : null] })) : null] }));
}
function ToolDetail({ label, ariaLabel, value, labelClassName, preClassName }) {
    return _jsxs("div", { children: [_jsx("span", { className: cx("ui-chat-tool__label", labelClassName), children: label }), _jsx("pre", { className: preClassName, "aria-label": ariaLabel, children: value })] });
}
function toneMark(tone) {
    if (tone === "done")
        return "✓";
    if (tone === "error")
        return "×";
    if (tone === "running")
        return "◌";
    return "◇";
}
export function aggregateToolCallTone(calls) {
    if (calls.some((call) => call.tone === "error"))
        return "error";
    if (calls.some((call) => call.tone === "running" || call.tone === "pending"))
        return "running";
    return "done";
}
export function ToolCallGroup({ calls, summaryLabel, labels, defaultOpen, icon, expandIcon, className, classes, callClasses }) {
    const tone = aggregateToolCallTone(calls);
    const [open, setOpen] = useState(defaultOpen ?? tone !== "done");
    const single = calls[0];
    if (calls.length === 1 && single)
        return _jsx(ToolCallCard, { ...single, labels: labels, classes: callClasses });
    if (calls.length === 0)
        return null;
    return (_jsxs("div", { className: cx("ui-chat-tool-group", className), "data-tone": tone, children: [_jsxs("button", { className: classes?.header, type: "button", "aria-expanded": open, onClick: () => setOpen((value) => !value), children: [_jsx("span", { className: classes?.icon, "aria-hidden": "true", children: icon ?? "◇" }), _jsx("span", { className: classes?.summary, children: summaryLabel }), _jsx("i", { className: classes?.dot, "aria-hidden": "true" }), _jsx("span", { className: cx(classes?.chevron, open && "is-open"), "data-open": open || undefined, "aria-hidden": "true", children: expandIcon ?? (open ? "⌄" : "›") })] }), open ? _jsx("div", { className: classes?.body, children: calls.map((call) => _jsx(ToolCallCard, { ...call, labels: labels, classes: callClasses }, call.id)) }) : null] }));
}
