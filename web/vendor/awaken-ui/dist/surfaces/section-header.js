import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
import { cx } from "../internal/cx.js";
export function SectionHeader({ title, icon, count, actions, as: Heading = "h2", className }) {
    return (_jsxs("div", { className: cx("ui-section-head", className), children: [_jsxs(Heading, { className: "ui-section-head__title", children: [icon ? _jsx("span", { className: "ui-section-head__icon", children: icon }) : null, _jsx("span", { children: title }), count == null ? null : _jsx("span", { className: "ui-section-head__count", children: count })] }), actions ? _jsx("div", { className: "ui-section-head__actions", children: actions }) : null] }));
}
//# sourceMappingURL=section-header.js.map