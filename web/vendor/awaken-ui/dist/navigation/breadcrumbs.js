import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
import { cloneElement } from "react";
import { cx } from "../internal/cx.js";
export function Breadcrumbs({ label, className, children, ...props }) {
    return _jsx("nav", { ...props, className: cx("ui-breadcrumbs", className), "aria-label": label, children: _jsx("ol", { className: "ui-breadcrumbs__list", children: children }) });
}
export function BreadcrumbItem(itemProps) {
    const { current = false, separator = "/", className, children } = itemProps;
    let content;
    if (current) {
        const { current: _current, separator: _separator, ...props } = itemProps;
        content = _jsx("span", { ...props, className: cx("ui-breadcrumbs__current", className), "aria-current": "page", children: children });
    }
    else {
        const { current: _current, separator: _separator, render, ...props } = itemProps;
        content = render
            ? cloneElement(render, { ...props, className: cx("ui-breadcrumbs__link", className, render.props.className) }, children)
            : _jsx("a", { ...props, className: cx("ui-breadcrumbs__link", className), children: children });
    }
    return _jsxs("li", { className: "ui-breadcrumbs__item", children: [content, _jsx("span", { className: "ui-breadcrumbs__separator", "aria-hidden": "true", children: separator })] });
}
//# sourceMappingURL=breadcrumbs.js.map