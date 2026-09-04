import { jsx as _jsx } from "react/jsx-runtime";
import { createElement as _createElement } from "react";
import { cx } from "../internal/cx.js";
export function SegmentedControl({ options, value, onChange, className, buttonClassName, activeClassName, inactiveClassName, buttonStyle, as: Container = "div", ariaLabel, activeDataAttribute, }) {
    return (_jsx(Container, { className: cx("ui-segmented", className), role: "group", "aria-label": ariaLabel, children: options.map((option) => {
            const active = option.value === value;
            const activeData = activeDataAttribute
                ? { [activeDataAttribute]: active || undefined }
                : {};
            return _createElement("button", { ...activeData, key: String(option.value), type: "button", className: cx("ui-segmented__btn", buttonClassName, active ? cx("is-active", activeClassName) : inactiveClassName), style: buttonStyle, "aria-pressed": active, "aria-label": option.ariaLabel, disabled: option.disabled, onClick: () => onChange(option.value) }, option.label);
        }) }));
}
