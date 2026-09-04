import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
import { forwardRef, useId, } from "react";
import { HeadlessDialog } from "../internal/headless/dialog.js";
import { cx } from "../internal/cx.js";
import { Button } from "../primitives/button.js";
/** A modal side panel for detail and management surfaces. */
export const Drawer = forwardRef(function Drawer({ children, className, classes, closeLabel, closeIcon, closeOnOutsidePress = true, description, footer, onOpenChange, open, side = "end", size = "md", title, titleId: suppliedTitleId, ...props }, ref) {
    const generatedTitleId = useId();
    const titleId = suppliedTitleId ?? generatedTitleId;
    const descriptionId = useId();
    return (_jsx(HeadlessDialog.Root, { disablePointerDismissal: !closeOnOutsidePress, onOpenChange: (nextOpen) => onOpenChange(nextOpen), open: open, children: _jsxs(HeadlessDialog.Portal, { children: [_jsx(HeadlessDialog.Backdrop, { className: cx("ui-overlay__backdrop", classes?.backdrop) }), _jsx(HeadlessDialog.Viewport, { className: cx("ui-drawer__viewport", classes?.viewport), children: _jsxs(HeadlessDialog.Popup, { ...props, "aria-describedby": description ? descriptionId : undefined, "aria-labelledby": titleId, className: cx("ui-drawer", classes?.panel, className), "data-side": side, "data-size": size, ref: ref, children: [_jsxs("header", { className: cx("ui-dialog__header", classes?.header), children: [_jsxs("div", { className: "ui-dialog__heading", children: [_jsx(HeadlessDialog.Title, { className: cx("ui-dialog__title", classes?.title), id: titleId, children: title }), description ? (_jsx(HeadlessDialog.Description, { className: "ui-dialog__description", id: descriptionId, children: description })) : null] }), _jsx(HeadlessDialog.Close, { "aria-label": closeLabel, render: _jsx(Button, { className: classes?.closeButton, variant: "icon", size: "sm" }), children: closeIcon ?? _jsx("span", { "aria-hidden": "true", children: "\u00D7" }) })] }), _jsx("div", { className: cx("ui-dialog__body", classes?.body), children: children }), footer ? _jsx("footer", { className: cx("ui-dialog__footer", classes?.footer), children: footer }) : null] }) })] }) }));
});
