import { jsx as _jsx } from "react/jsx-runtime";
import { cx } from "../internal/cx.js";
export function Panel({ accent = "plain", className, ...props }) {
    return _jsx("aside", { ...props, className: cx("ui-panel", accent === "agent" && "ui-panel--agent", className) });
}
export function PanelHeader({ className, ...props }) {
    return _jsx("div", { ...props, className: cx("ui-panel__header", className) });
}
export function PanelBody({ className, ...props }) {
    return _jsx("div", { ...props, className: cx("ui-panel__body", className) });
}
