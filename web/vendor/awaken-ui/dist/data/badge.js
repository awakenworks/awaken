import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
import { cx } from "../internal/cx.js";
export function Badge({ children, className, tone = "neutral", ...props }) {
    return _jsx("span", { ...props, className: cx("ui-badge", `ui-badge--${tone}`, className), children: children });
}
export function StatusPill({ children, className, tone = "neutral", ...props }) {
    return _jsx("span", { ...props, className: cx("ui-status-pill", `ui-status-pill--${tone}`, className), children: children });
}
export function Chip({ children, className, icon, tone = "neutral", ...props }) {
    return _jsxs("span", { ...props, className: cx("ui-chip", `ui-chip--${tone}`, className), children: [icon, children] });
}
