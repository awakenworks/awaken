import { jsx as _jsx } from "react/jsx-runtime";
import { cx } from "../internal/cx.js";
export function Switch({ className, label, onChange, onCheckedChange, ...props }) {
    const handleChange = (event) => {
        onChange?.(event);
        if (!event.defaultPrevented) {
            onCheckedChange?.(event.currentTarget.checked);
        }
    };
    return (_jsx("input", { ...props, type: "checkbox", role: "switch", "aria-label": props["aria-label"] ?? label, className: cx("ui-switch", className), onChange: handleChange }));
}
