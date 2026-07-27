import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
import { cx } from "../internal/cx.js";
/** Controlled multi-select list; option order is preserved in emitted values. */
export function CheckPicker({ options, selected, onChange, empty = "—", className, classes, }) {
    const toggle = (id) => {
        onChange(selected.includes(id)
            ? selected.filter((value) => value !== id)
            : [...selected, id]);
    };
    if (options.length === 0) {
        return _jsx("span", { className: cx("ui-check-picker__empty", classes?.empty), children: empty });
    }
    return (_jsx("div", { className: cx("ui-check-picker", classes?.root, className), children: options.map((option) => (_jsxs("label", { className: cx("ui-check-picker__row", classes?.row), children: [_jsx("input", { checked: selected.includes(option.id), disabled: option.disabled, onChange: () => toggle(option.id), type: "checkbox" }), _jsx("span", { className: cx("ui-check-picker__label", classes?.label), children: option.label ?? option.id }), option.description ? (_jsx("span", { className: cx("ui-check-picker__description", classes?.description), children: option.description })) : null] }, option.id))) }));
}
//# sourceMappingURL=check-picker.js.map