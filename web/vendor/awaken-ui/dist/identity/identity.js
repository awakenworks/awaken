import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
import { cx } from "../internal/cx.js";
import { Avatar } from "./avatar.js";
/** Canonical media → name → supporting information identity skeleton. */
export function Identity({ name, description, media, avatar, status, badges, metadata, actions, classes, className, ...props }) {
    if (media === undefined && avatar === undefined) {
        throw new Error("Identity requires either media or avatar.");
    }
    return (_jsxs("div", { ...props, className: cx("ui-identity", className), children: [_jsx("div", { className: cx("ui-identity__media", classes?.media), children: media ?? (avatar ? _jsx(Avatar, { ...avatar }) : null) }), _jsxs("div", { className: cx("ui-identity__body", classes?.body), children: [_jsxs("div", { className: cx("ui-identity__heading", classes?.heading), children: [_jsx("div", { className: cx("ui-identity__name", classes?.name), children: name }), status === undefined ? null : _jsx("div", { className: cx("ui-identity__status", classes?.status), children: status })] }), description === undefined ? null : (_jsx("div", { className: cx("ui-identity__description", classes?.description), children: description })), badges === undefined ? null : _jsx("div", { className: cx("ui-identity__badges", classes?.badges), children: badges }), metadata === undefined ? null : _jsx("div", { className: cx("ui-identity__metadata", classes?.metadata), children: metadata })] }), actions === undefined ? null : _jsx("div", { className: cx("ui-identity__actions", classes?.actions), children: actions })] }));
}
/** Surface wrapper; routing and domain actions remain consumer-owned. */
export function IdentityCard({ selected = false, href, onActivate, className, ...identity }) {
    const content = _jsx(Identity, { ...identity });
    const interactive = href !== undefined || onActivate !== undefined;
    return (_jsx("article", { className: cx("ui-identity-card", className), "data-interactive": interactive || undefined, "data-selected": selected || undefined, children: href !== undefined ? (_jsx("a", { className: "ui-identity-card__target", href: href, onClick: onActivate, children: content })) : onActivate !== undefined ? (_jsx("button", { className: "ui-identity-card__target", type: "button", onClick: onActivate, children: content })) : content }));
}
