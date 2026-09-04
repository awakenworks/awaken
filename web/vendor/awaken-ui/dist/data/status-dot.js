import { jsx as _jsx } from "react/jsx-runtime";
import { cx } from "../internal/cx.js";
export function StatusDot({ tone, color, className, title, label }) {
    const style = color ? { background: color } : undefined;
    return (_jsx("span", { className: cx(tone && `health-dot health-dot--${tone}`, className) || undefined, style: style, title: title, "aria-label": label, "aria-hidden": label || title ? undefined : true }));
}
