import { jsx as _jsx } from "react/jsx-runtime";
import { createContext, useContext, useEffect, useId, useRef, } from "react";
import { cx } from "../internal/cx.js";
const TabsContext = createContext(null);
function useTabsContext() {
    const value = useContext(TabsContext);
    if (!value)
        throw new Error("Tabs parts must be rendered inside Tabs.");
    return value;
}
function partId(root, part, value) {
    return `${root}-${part}-${encodeURIComponent(value)}`;
}
export function Tabs({ value, onValueChange, activationMode = "automatic", orientation = "horizontal", className, children, ...props }) {
    const id = useId();
    return _jsx(TabsContext.Provider, { value: { value, onValueChange, activationMode, orientation, id }, children: _jsx("div", { ...props, className: cx("ui-tabs", className), "data-orientation": orientation, children: children }) });
}
export function TabList({ className, ...props }) {
    const { orientation } = useTabsContext();
    const listRef = useRef(null);
    useEffect(() => {
        const tabs = Array.from(listRef.current?.querySelectorAll("[role=tab]:not(:disabled)") ?? []);
        if (tabs.length > 0 && !tabs.some((tab) => tab.tabIndex === 0))
            tabs[0]?.setAttribute("tabindex", "0");
    });
    return _jsx("div", { ref: listRef, ...props, className: cx("ui-tabs__list", className), role: "tablist", "aria-orientation": orientation });
}
export function Tab({ value, className, disabled, onClick, onKeyDown, ...props }) {
    const context = useTabsContext();
    const selected = context.value === value;
    const handleKeyDown = (event) => {
        onKeyDown?.(event);
        if (event.defaultPrevented)
            return;
        const forward = context.orientation === "horizontal" ? "ArrowRight" : "ArrowDown";
        const backward = context.orientation === "horizontal" ? "ArrowLeft" : "ArrowUp";
        if (![forward, backward, "Home", "End"].includes(event.key))
            return;
        const list = event.currentTarget.closest("[role=tablist]");
        const tabs = Array.from(list?.querySelectorAll("[role=tab]:not(:disabled)") ?? []);
        const current = tabs.indexOf(event.currentTarget);
        if (current < 0 || tabs.length === 0)
            return;
        event.preventDefault();
        const nextIndex = event.key === "Home" ? 0 : event.key === "End" ? tabs.length - 1
            : event.key === forward ? (current + 1) % tabs.length
                : (current - 1 + tabs.length) % tabs.length;
        const next = tabs[nextIndex];
        next?.focus();
        if (context.activationMode === "automatic")
            next?.click();
    };
    return _jsx("button", { ...props, type: "button", role: "tab", id: partId(context.id, "tab", value), "aria-controls": partId(context.id, "panel", value), "aria-selected": selected, disabled: disabled, tabIndex: selected && !disabled ? 0 : -1, className: cx("ui-tabs__tab", className), onKeyDown: handleKeyDown, onClick: (event) => { onClick?.(event); if (!event.defaultPrevented)
            context.onValueChange(value); } });
}
export function TabPanel({ value, className, ...props }) {
    const context = useTabsContext();
    const selected = context.value === value;
    return _jsx("div", { ...props, role: "tabpanel", id: partId(context.id, "panel", value), "aria-labelledby": partId(context.id, "tab", value), className: cx("ui-tabs__panel", className), hidden: !selected, tabIndex: 0 });
}
