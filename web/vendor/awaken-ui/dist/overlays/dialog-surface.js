import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
import { createElement, useCallback, useEffect, useRef, } from "react";
import { HeadlessDialog } from "../internal/headless/dialog.js";
/**
 * Behavior-only modal boundary for product-specific surfaces. Products own
 * markup classes and tokens; Base UI owns focus, Escape, outside dismissal,
 * portal lifecycle, scroll locking, and focus restoration.
 */
export function DialogSurface({ open, onOpenChange, children, rootClassName, panelClassName, overlayClassName, panelAs = "div", panelRef, labelledBy, ariaLabel, closeOnBackdrop = true, panelProps, }) {
    const returnFocusRef = useRef(null);
    const rootRef = useRef(null);
    const activePanelRef = useRef(null);
    const setPanelRef = useCallback((node) => {
        if (panelRef)
            panelRef.current = node;
        activePanelRef.current = node;
        if (!node || !open)
            return;
        returnFocusRef.current =
            document.activeElement instanceof HTMLElement ? document.activeElement : null;
        node
            .querySelector('button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])')
            ?.focus();
    }, [open, panelRef]);
    useEffect(() => {
        if (!open)
            return;
        const onKeyDown = (event) => {
            if (event.key !== "Tab")
                return;
            const panel = activePanelRef.current;
            if (!panel)
                return;
            const focusable = Array.from(panel.querySelectorAll('button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])'));
            const first = focusable[0];
            const last = focusable[focusable.length - 1];
            if (event.shiftKey && document.activeElement === first) {
                event.preventDefault();
                last?.focus();
            }
            else if (!event.shiftKey && document.activeElement === last) {
                event.preventDefault();
                first?.focus();
            }
        };
        document.addEventListener("keydown", onKeyDown);
        return () => document.removeEventListener("keydown", onKeyDown);
    }, [open]);
    if (!open)
        return null;
    return (_jsx("div", { className: rootClassName, onMouseDown: (event) => {
            if (closeOnBackdrop && event.target === event.currentTarget) {
                onOpenChange(false);
            }
        }, ref: rootRef, role: "presentation", children: _jsx(HeadlessDialog.Root, { disablePointerDismissal: !closeOnBackdrop, onOpenChange: (nextOpen) => {
                onOpenChange(nextOpen);
                if (!nextOpen)
                    returnFocusRef.current?.focus();
            }, open: open, children: _jsxs(HeadlessDialog.Portal, { className: "ui-dialog-surface__portal", container: rootRef, children: [overlayClassName ? (_jsx(HeadlessDialog.Backdrop, { className: overlayClassName, onMouseDown: closeOnBackdrop ? () => onOpenChange(false) : undefined })) : null, _jsx(HeadlessDialog.Viewport, { className: "ui-dialog-surface__viewport", children: _jsx(HeadlessDialog.Popup, { ...panelProps, "aria-label": ariaLabel, "aria-labelledby": labelledBy, className: panelClassName, ref: setPanelRef, render: createElement(panelAs), children: children }) })] }) }) }));
}
//# sourceMappingURL=dialog-surface.js.map