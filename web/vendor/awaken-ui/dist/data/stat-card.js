import { jsx as _jsx, jsxs as _jsxs, Fragment as _Fragment } from "react/jsx-runtime";
import { cloneElement, } from "react";
import { cx } from "../internal/cx.js";
export function StatCard({ value, label, icon, hint, tone = "neutral", variant = "tile", onClick, className, ariaLabel, render, }) {
    const classes = cx("ui-stat", `ui-stat--${variant}`, tone !== "neutral" && `ui-stat--${tone}`, className);
    const content = (_jsxs(_Fragment, { children: [icon ? _jsx("span", { className: "ui-stat__icon", children: icon }) : null, _jsxs("span", { className: "ui-stat__body", children: [_jsx("span", { className: "ui-stat__value", children: value }), _jsx("span", { className: "ui-stat__label", children: label }), hint ? _jsx("span", { className: "ui-stat__hint", children: hint }) : null] })] }));
    if (render) {
        return cloneElement(render, {
            "aria-label": ariaLabel,
            className: cx(classes, render.props.className),
            onClick,
        }, content);
    }
    if (onClick) {
        return (_jsx("button", { type: "button", className: classes, "aria-label": ariaLabel, onClick: onClick, children: content }));
    }
    return _jsx("div", { className: classes, "aria-label": ariaLabel, children: content });
}
export function StatGrid({ className, ...props }) {
    return _jsx("div", { ...props, className: cx("ui-stat-grid", className) });
}
//# sourceMappingURL=stat-card.js.map