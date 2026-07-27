import { jsx as _jsx } from "react/jsx-runtime";
import { cx } from "../internal/cx.js";
export function ToolbarRow({ className, ...props }) {
    return _jsx("div", { ...props, className: cx("ui-toolbar", className), role: props.role ?? "toolbar" });
}
export function Stack({ className, ...props }) {
    return _jsx("div", { ...props, className: cx("ui-stack", className) });
}
export function Cluster({ className, ...props }) {
    return _jsx("div", { ...props, className: cx("ui-cluster", className) });
}
export function SplitPane({ className, ...props }) {
    return _jsx("div", { ...props, className: cx("ui-split-pane", className) });
}
//# sourceMappingURL=layout.js.map