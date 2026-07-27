import { jsx as _jsx } from "react/jsx-runtime";
import { cx } from "../internal/cx.js";
export function Toolbar({ className, ...props }) {
    return _jsx("div", { ...props, className: cx("surface-toolbar", className) });
}
export function ToolbarLead({ className, ...props }) {
    return _jsx("span", { ...props, className: cx("surface-toolbar-lead", className) });
}
export function ToolbarSpacer({ className, ...props }) {
    return _jsx("div", { ...props, "aria-hidden": "true", className: cx("surface-toolbar__spacer", className) });
}
//# sourceMappingURL=toolbar.js.map