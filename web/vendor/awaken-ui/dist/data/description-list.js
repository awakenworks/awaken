import { jsx as _jsx } from "react/jsx-runtime";
import { cx } from "../internal/cx.js";
export function DescriptionList({ columns = 1, density = "default", className, ...props }) {
    return _jsx("dl", { ...props, className: cx("ui-description-list", className), "data-columns": columns, "data-density": density });
}
export function DescriptionItem({ className, ...props }) {
    return _jsx("div", { ...props, className: cx("ui-description-list__item", className) });
}
export function DescriptionTerm({ className, ...props }) {
    return _jsx("dt", { ...props, className: cx("ui-description-list__term", className) });
}
export function DescriptionDetails({ className, ...props }) {
    return _jsx("dd", { ...props, className: cx("ui-description-list__details", className) });
}
