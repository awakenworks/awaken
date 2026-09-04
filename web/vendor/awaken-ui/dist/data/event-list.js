import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
import { cx } from "../internal/cx.js";
export function EventList({ density = "default", className, ...props }) {
    return _jsx("ol", { ...props, className: cx("ui-event-list", className), "data-density": density });
}
export function EventItem({ marker, title, timestamp, metadata, actions, children, className, ...props }) {
    return _jsxs("li", { ...props, className: cx("ui-event-list__item", className), children: [_jsx("span", { className: "ui-event-list__rail", "aria-hidden": "true", children: _jsx("span", { className: "ui-event-list__marker", children: marker }) }), _jsxs("div", { className: "ui-event-list__content", children: [_jsxs("div", { className: "ui-event-list__header", children: [_jsx("span", { className: "ui-event-list__title", children: title }), timestamp] }), metadata !== undefined && metadata !== null ? _jsx("div", { className: "ui-event-list__metadata", children: metadata }) : null, children !== undefined && children !== null ? _jsx("div", { className: "ui-event-list__body", children: children }) : null] }), actions !== undefined && actions !== null ? _jsx("div", { className: "ui-event-list__actions", children: actions }) : null] });
}
export function EventTime({ className, ...props }) {
    return _jsx("time", { ...props, className: cx("ui-event-list__time", className) });
}
