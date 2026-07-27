import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
import { useEffect, useState } from "react";
import { cx } from "../internal/cx.js";
export function ChatThinking({ label, formatElapsed, icon, className, classes }) {
    const [elapsed, setElapsed] = useState(0);
    useEffect(() => {
        const timer = globalThis.setInterval(() => setElapsed((value) => value + 1), 1000);
        return () => globalThis.clearInterval(timer);
    }, []);
    return (_jsxs("div", { className: cx("ui-chat-thinking", className), role: "status", "aria-live": "polite", children: [_jsx("span", { className: cx("ui-chat-thinking__icon", classes?.icon), "aria-hidden": "true", children: icon ?? "◌" }), _jsxs("span", { className: classes?.label, children: [label, elapsed > 0 && formatElapsed ? ` · ${formatElapsed(elapsed)}` : ""] }), _jsxs("span", { className: cx("ui-chat-thinking__dots", classes?.dots), "aria-hidden": "true", children: [_jsx("i", {}), _jsx("i", {}), _jsx("i", {})] })] }));
}
export function ReasoningBlock({ label, children, defaultOpen = false, streaming = false, icon, expandIcon, className, classes, }) {
    const [open, setOpen] = useState(defaultOpen);
    return (_jsxs("section", { className: cx("ui-chat-reasoning", className), children: [_jsxs("button", { className: classes?.header, type: "button", "aria-expanded": open, onClick: () => setOpen((value) => !value), children: [_jsx("span", { className: classes?.icon, "aria-hidden": "true", children: icon ?? "◇" }), _jsx("span", { children: label }), _jsx("span", { className: cx(classes?.chevron, open && "is-open"), "data-open": open || undefined, "aria-hidden": "true", children: expandIcon ?? (open ? "⌄" : "›") })] }), open ? _jsxs("div", { className: classes?.body, children: [children, streaming ? _jsx("span", { className: classes?.caret, "aria-hidden": "true", children: "\u258D" }) : null] }) : null] }));
}
//# sourceMappingURL=status.js.map