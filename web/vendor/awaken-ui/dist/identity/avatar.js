import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
import { cx } from "../internal/cx.js";
export function initialsOf(label) {
    const words = label.trim().split(/\s+/).filter(Boolean);
    if (words.length === 0)
        return "?";
    const first = words[0]?.[0] ?? "";
    const last = words.length > 1 ? words.at(-1)?.[0] ?? "" : "";
    return `${first}${last}`.toLocaleUpperCase();
}
/** Product-neutral identity carrier. Custom generated marks use `children`. */
export function Avatar({ label, size = "md", src, initials, children, classes, className, ...props }) {
    const customContent = children !== undefined;
    return (_jsxs("span", { ...props, role: "img", "aria-label": label, className: cx("ui-avatar", className), "data-size": size, children: [src ? _jsx("img", { className: cx("ui-avatar__image", classes?.image), src: src, alt: "" }) : null, !src && customContent ? children : null, !src && !customContent ? (_jsx("span", { className: classes?.fallback, "aria-hidden": "true", children: initials?.trim() || initialsOf(label) })) : null] }));
}
export function AvatarGroup({ children, className, overflow = 0, overflowLabel = (count) => `+${count}`, avatarClasses, ...props }) {
    return (_jsxs("span", { ...props, className: cx("ui-avatar-group", className), children: [children, overflow > 0 ? (_jsx(Avatar, { className: "ui-avatar--overflow", initials: `+${overflow}`, label: overflowLabel(overflow), ...(avatarClasses ? { classes: avatarClasses } : {}) })) : null] }));
}
//# sourceMappingURL=avatar.js.map