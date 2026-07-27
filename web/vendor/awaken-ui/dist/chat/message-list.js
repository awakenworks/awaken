import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
import { useCallback, useEffect, useRef, useState } from "react";
import { cx } from "../internal/cx.js";
export const CHAT_STICK_THRESHOLD = 80;
export function isNearChatBottom(scrollTop, scrollHeight, clientHeight, threshold = CHAT_STICK_THRESHOLD) {
    return scrollHeight - (scrollTop + clientHeight) <= threshold;
}
/** Follows streaming output only while the reader remains near the bottom. */
export function ChatMessageList({ children, ariaLabel, jumpLabel, busy = false, className, viewportClassName, jumpClassName, jumpIcon, }) {
    const viewportRef = useRef(null);
    const followingRef = useRef(true);
    const [showJump, setShowJump] = useState(false);
    const scrollToBottom = useCallback((behavior) => {
        const viewport = viewportRef.current;
        if (!viewport)
            return;
        viewport.scrollTo?.({ top: viewport.scrollHeight, behavior });
        if (typeof viewport.scrollTo !== "function")
            viewport.scrollTop = viewport.scrollHeight;
    }, []);
    useEffect(() => {
        const viewport = viewportRef.current;
        if (!viewport || typeof MutationObserver === "undefined")
            return;
        const observer = new MutationObserver(() => {
            if (followingRef.current)
                scrollToBottom("auto");
        });
        observer.observe(viewport, { childList: true, subtree: true, characterData: true });
        return () => observer.disconnect();
    }, [scrollToBottom]);
    return (_jsxs("div", { className: cx("ui-chat-list", className), children: [_jsx("div", { ref: viewportRef, className: cx("ui-chat-list__viewport", viewportClassName), role: "log", "aria-label": ariaLabel, "aria-busy": busy, "aria-live": "polite", onScroll: (event) => {
                    const node = event.currentTarget;
                    const following = isNearChatBottom(node.scrollTop, node.scrollHeight, node.clientHeight);
                    followingRef.current = following;
                    setShowJump(!following);
                }, children: children }), showJump ? (_jsxs("button", { className: cx("ui-chat-list__jump", jumpClassName), type: "button", onClick: () => {
                    followingRef.current = true;
                    setShowJump(false);
                    scrollToBottom("smooth");
                }, children: [_jsx("span", { "aria-hidden": "true", children: jumpIcon ?? "↓" }), " ", jumpLabel] })) : null] }));
}
//# sourceMappingURL=message-list.js.map