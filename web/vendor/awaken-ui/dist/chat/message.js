import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
import { cx } from "../internal/cx.js";
import { Avatar } from "../identity/avatar.js";
export function formatChatTime(timestamp) {
    if (!timestamp)
        return "";
    const date = new Date(timestamp);
    if (Number.isNaN(date.getTime()))
        return "";
    return `${String(date.getHours()).padStart(2, "0")}:${String(date.getMinutes()).padStart(2, "0")}`;
}
export function ChatMessage({ role, authorLabel, body, media, timestamp, actions, children, compact = false, className, classes, }) {
    const stamp = formatChatTime(timestamp);
    return (_jsxs("article", { className: cx("ui-chat-message", className), "data-role": role, "data-compact": compact || undefined, children: [_jsx("div", { className: cx("ui-chat-message__media", classes?.media), children: media ?? _jsx(Avatar, { label: authorLabel, size: "sm" }) }), _jsxs("div", { className: cx("ui-chat-message__content", classes?.content), children: [_jsxs("header", { className: cx("ui-chat-message__header", classes?.header), children: [_jsx("strong", { className: classes?.author, children: authorLabel }), stamp ? _jsx("time", { className: classes?.time, dateTime: timestamp, children: stamp }) : null, actions] }), body === undefined ? null : _jsx("div", { className: cx("ui-chat-message__body", classes?.body), children: body }), children] })] }));
}
