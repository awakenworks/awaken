import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
import { useState, useRef, useCallback, } from "react";
import { HeadlessPopover } from "../internal/headless/popover.js";
import { cx } from "../internal/cx.js";
export function MenuPopover(props) {
    return _jsx(Popover, { ...props, role: "menu" });
}
/**
 * Product-neutral anchored surface. Base UI owns positioning, focus,
 * dismissal, and trigger ARIA; this layer adds the shared menu keyboard and
 * close-after-action contracts used across products.
 */
export function Popover({ children, content, placement = "bottom-start", closeOnContentClick = false, "aria-label": ariaLabel, role = "dialog", className, contentClassName, contentId, rootProps, closeOnMouseLeave = false, open: controlledOpen, defaultOpen = false, onOpenChange, }) {
    const [uncontrolledOpen, setUncontrolledOpen] = useState(defaultOpen);
    const open = controlledOpen ?? uncontrolledOpen;
    const setOpen = (nextOpen) => {
        if (controlledOpen === undefined)
            setUncontrolledOpen(nextOpen);
        onOpenChange?.(nextOpen);
    };
    const rootRef = useRef(null);
    const panelRef = useRef(null);
    const triggerRef = useRef(null);
    const align = placement === "bottom-end" ? "end" : "start";
    const setPanelRef = useCallback((node) => {
        panelRef.current = node;
        if (!node || !open)
            return;
        node
            .querySelector('button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])')
            ?.focus();
    }, [open]);
    const onPanelKeyDown = (event) => {
        if (role === "dialog")
            return;
        if (!["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key))
            return;
        const items = Array.from(event.currentTarget.querySelectorAll('button:not([disabled]), [role="menuitem"]:not([aria-disabled="true"]), [role="option"]:not([aria-disabled="true"])'));
        if (items.length === 0)
            return;
        event.preventDefault();
        const current = items.indexOf(document.activeElement);
        const next = event.key === "Home"
            ? 0
            : event.key === "End"
                ? items.length - 1
                : event.key === "ArrowDown"
                    ? current < 0
                        ? 0
                        : (current + 1) % items.length
                    : current <= 0
                        ? items.length - 1
                        : current - 1;
        items[next]?.focus();
    };
    return (_jsx(HeadlessPopover.Root, { modal: "trap-focus", onOpenChange: (nextOpen) => {
            setOpen(nextOpen);
            if (!nextOpen)
                triggerRef.current?.focus();
        }, open: open, children: _jsxs("div", { ...rootProps, className: cx("ui-popover", className), onMouseLeave: (event) => {
                rootProps?.onMouseLeave?.(event);
                if (closeOnMouseLeave)
                    setOpen(false);
            }, ref: rootRef, children: [_jsx(HeadlessPopover.Trigger, { "aria-haspopup": role, ref: triggerRef, render: children, nativeButton: true }), _jsx(HeadlessPopover.Portal, { container: rootRef, children: _jsx(HeadlessPopover.Positioner, { align: align, className: "ui-popover__positioner", side: "bottom", sideOffset: 8, children: _jsx(HeadlessPopover.Popup, { "aria-label": ariaLabel, className: cx("ui-popover__content", `ui-popover__content--${placement}`, contentClassName), id: contentId, initialFocus: true, onClick: closeOnContentClick
                                ? ((event) => {
                                    if (event.target.closest('button, a[href], [role="menuitem"], [role="option"]')) {
                                        setOpen(false);
                                    }
                                })
                                : undefined, onKeyDown: onPanelKeyDown, ref: setPanelRef, role: role, children: content }) }) })] }) }));
}
