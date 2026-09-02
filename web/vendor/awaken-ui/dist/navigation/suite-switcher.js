import { jsx as _jsx, jsxs as _jsxs, Fragment as _Fragment } from "react/jsx-runtime";
import { MenuPopover } from "../overlays/popover.js";
/**
 * Product-neutral suite navigation presentation.
 *
 * Consumers own every label, icon, URL and authorization decision. The shared
 * component owns the accessible menu, current-item treatment, focus/keyboard
 * behavior and the one consistent product/destination layout.
 */
export function SuiteSwitcher({ "aria-label": ariaLabel, currentLabel, destinations = [], placement = "bottom-start", products, trigger, }) {
    return (_jsx(MenuPopover, { "aria-label": ariaLabel, closeOnContentClick: true, content: (_jsxs("div", { className: "ui-suite-switcher", children: [_jsx("div", { className: "ui-suite-switcher__current-label", children: currentLabel }), _jsx("div", { className: "ui-suite-switcher__products", children: products.map((product) => product.isCurrent || !product.href
                        ? _jsx("div", { "aria-current": product.isCurrent ? "page" : undefined, "aria-disabled": "true", className: "ui-suite-switcher__item", "data-current": product.isCurrent || undefined, role: "menuitem", children: _jsx(SuiteItemContent, { icon: product.icon, label: product.label, description: product.description }) }, product.id)
                        : _jsxs("a", { className: "ui-suite-switcher__item", href: product.href, role: "menuitem", children: [_jsx(SuiteItemContent, { icon: product.icon, label: product.label, description: product.description }), _jsx("span", { "aria-hidden": "true", className: "ui-suite-switcher__arrow", children: "\u2192" })] }, product.id)) }), destinations.length > 0 ? _jsx("div", { className: "ui-suite-switcher__destinations", children: destinations.map((destination) => _jsxs("a", { className: "ui-suite-switcher__item", href: destination.href, role: "menuitem", children: [_jsx(SuiteItemContent, { icon: destination.icon, label: destination.label, description: destination.description }), _jsx("span", { "aria-hidden": "true", className: "ui-suite-switcher__arrow", children: "\u2192" })] }, destination.id)) }) : null] })), placement: placement, children: trigger }));
}
function SuiteItemContent({ description, icon, label, }) {
    return _jsxs(_Fragment, { children: [icon ? _jsx("span", { "aria-hidden": "true", className: "ui-suite-switcher__icon", children: icon }) : null, _jsxs("span", { className: "ui-suite-switcher__copy", children: [_jsx("strong", { children: label }), description ? _jsx("small", { children: description }) : null] })] });
}
//# sourceMappingURL=suite-switcher.js.map
