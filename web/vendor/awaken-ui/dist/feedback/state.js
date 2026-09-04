import { jsx as _jsx, jsxs as _jsxs, Fragment as _Fragment } from "react/jsx-runtime";
import { Button } from "../primitives/button.js";
export function EmptyState(props) {
    return _jsx(StateBlock, { ...props, tone: "neutral" });
}
export function ErrorState(props) {
    return _jsx(StateBlock, { ...props, tone: "danger" });
}
export function LoadingState({ label, icon }) {
    return _jsxs("div", { className: "ui-state ui-state--loading", role: "status", children: [icon ? _jsx("span", { className: "ui-state__icon", children: icon }) : null, _jsx("p", { children: label })] });
}
export function LoadingRow({ label, icon }) {
    return _jsxs("div", { className: "ui-state ui-state--neutral", role: "status", children: [icon ? _jsx("span", { className: "ui-state__icon", children: icon }) : null, _jsx("p", { children: label })] });
}
export function SkeletonList({ rows = 6, label }) {
    return _jsx("div", { className: "ui-skeleton", role: "status", "aria-label": label, children: Array.from({ length: rows }, (_, index) => _jsxs("div", { className: "ui-skeleton-row", children: [_jsx("span", { className: "ui-skeleton-bar ui-skeleton-bar--dot" }), _jsxs("span", { className: "ui-skeleton-lines", children: [_jsx("span", { className: "ui-skeleton-bar ui-skeleton-bar--wide" }), _jsx("span", { className: "ui-skeleton-bar ui-skeleton-bar--narrow" })] })] }, index)) });
}
export function Skeleton({ width = "100%", height = 12, className, }) {
    return _jsx("span", { "aria-hidden": "true", className: className ? `ui-skeleton-bar ${className}` : "ui-skeleton-bar", style: { width, height, display: "inline-block" } });
}
export function SurfaceGate({ query, isEmpty = false, loading, loadingContent, error, empty, loadingIcon, errorIcon, emptyIcon, children, }) {
    if (query.isLoading)
        return _jsx(_Fragment, { children: loadingContent ?? _jsx(LoadingState, { label: loading, icon: loadingIcon }) });
    if (query.isError)
        return _jsx(ErrorState, { title: error.title, body: error.body, icon: errorIcon, action: { label: error.retry, onClick: () => void query.refetch?.() } });
    if (isEmpty && empty)
        return _jsx(EmptyState, { ...empty, icon: empty.icon ?? emptyIcon });
    return _jsx(_Fragment, { children: children });
}
function StateBlock({ action, actions, body, className, icon, title, tone }) {
    return _jsxs("div", { className: `ui-state ui-state--${tone}${className ? ` ${className}` : ""}`, children: [icon ? _jsx("span", { className: "ui-state__icon", children: icon }) : null, _jsx("h2", { children: title }), body === undefined ? null : _jsx("p", { children: body }), action?.href ? _jsx("a", { className: "ui-button", "data-variant": "secondary", href: action.href, children: action.label })
                : action ? _jsx(Button, { onClick: action.onClick, variant: tone === "danger" ? "danger" : "secondary", children: action.label })
                    : null, actions] });
}
