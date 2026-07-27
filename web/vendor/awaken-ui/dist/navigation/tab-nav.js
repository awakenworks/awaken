import { jsx as _jsx } from "react/jsx-runtime";
import { cloneElement } from "react";
import { cx } from "../internal/cx.js";
export function TabNav({ label, className, children, ...props }) {
    return _jsx("nav", { ...props, className: cx("ui-tab-nav", className), "aria-label": label, children: _jsx("ul", { className: "ui-tab-nav__list", children: children }) });
}
export function TabNavItem({ current = false, render, className, children, ...props }) {
    const anchorProps = {
        ...props,
        className: cx("ui-tab-nav__link", className, render?.props.className),
        "aria-current": current ? "page" : undefined,
        children,
    };
    return _jsx("li", { className: "ui-tab-nav__item", "data-current": current || undefined, children: render ? cloneElement(render, anchorProps) : _jsx("a", { ...anchorProps }) });
}
//# sourceMappingURL=tab-nav.js.map