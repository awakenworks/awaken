import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
import { useCallback, useLayoutEffect, useRef, } from "react";
import { cx } from "../internal/cx.js";
export function resizeComposerToContent(textarea) {
    if (!textarea)
        return;
    textarea.style.height = "auto";
    textarea.style.height = `${textarea.scrollHeight}px`;
}
export function useAutoGrowingComposer(value) {
    const ref = useRef(null);
    const resize = useCallback(() => resizeComposerToContent(ref.current), []);
    useLayoutEffect(() => resize(), [resize, value]);
    return { ref, resize };
}
export function isComposerSubmitShortcut(event, mode) {
    if (event.key !== "Enter" || event.nativeEvent.isComposing)
        return false;
    return mode === "modifier-enter"
        ? event.ctrlKey || event.metaKey
        : !event.shiftKey && !event.ctrlKey && !event.metaKey && !event.altKey;
}
export function ChatComposer({ value, onChange, onSubmit, onStop, busy = false, disabled = false, sendMode = "modifier-enter", placeholder, ariaLabel, sendLabel, stopLabel, hint, leadingActions, sendIcon, stopIcon, className, classes, }) {
    const { ref } = useAutoGrowingComposer(value);
    const canSubmit = !disabled && !busy && value.trim().length > 0;
    const submit = () => {
        if (canSubmit)
            onSubmit();
    };
    const onFormSubmit = (event) => {
        event.preventDefault();
        submit();
    };
    const onKeyDown = (event) => {
        if (!isComposerSubmitShortcut(event, sendMode))
            return;
        event.preventDefault();
        submit();
    };
    return (_jsxs("form", { className: cx("ui-chat-composer", className), onSubmit: onFormSubmit, children: [_jsx("div", { className: cx("ui-chat-composer__input-wrap", classes?.inputWrapper), children: _jsx("textarea", { ref: ref, className: cx("ui-chat-composer__input", classes?.input), rows: 1, value: value, disabled: disabled, placeholder: placeholder, "aria-label": ariaLabel, onChange: (event) => onChange(event.target.value), onKeyDown: onKeyDown }) }), _jsxs("div", { className: cx("ui-chat-composer__controls", classes?.controls), children: [_jsx("div", { className: cx("ui-chat-composer__leading", classes?.leading), children: leadingActions }), _jsxs("div", { className: cx("ui-chat-composer__actions", classes?.actions), children: [onStop && stopLabel ? (_jsx("button", { className: classes?.stop, type: "button", disabled: !busy, onClick: onStop, "aria-label": stopLabel, title: stopLabel, children: stopIcon ?? _jsx("span", { "aria-hidden": "true", children: "\u25A0" }) })) : null, _jsx("button", { className: classes?.send, type: "submit", disabled: !canSubmit, "aria-label": sendLabel, title: sendLabel, children: sendIcon ?? _jsx("span", { "aria-hidden": "true", children: "\u2191" }) })] })] }), hint === undefined ? null : _jsx("div", { className: cx("ui-chat-composer__hint", classes?.hint), children: hint })] }));
}
