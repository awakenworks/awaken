import { jsx as _jsx, Fragment as _Fragment, jsxs as _jsxs } from "react/jsx-runtime";
import { forwardRef, } from "react";
import { cx } from "../internal/cx.js";
export const Button = forwardRef(function Button({ children, className, disabled, icon, loading = false, loadingLabel, size = "md", type = "button", variant = "default", ...props }, ref) {
    const isDisabled = disabled || loading;
    return (_jsxs("button", { ...props, "aria-busy": loading || undefined, className: cx("ui-button", className), "data-size": size, "data-variant": variant, disabled: isDisabled, ref: ref, type: type, children: [loading ? _jsx("span", { "aria-hidden": "true", className: "ui-button__spinner" }) : icon, loading && loadingLabel ? (_jsxs(_Fragment, { children: [_jsx("span", { className: "ui-visually-hidden", children: loadingLabel }), _jsx("span", { "aria-hidden": "true", children: children })] })) : (children)] }));
});
