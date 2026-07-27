import { Fragment as _Fragment, jsxs as _jsxs, jsx as _jsx } from "react/jsx-runtime";
import { cx } from "../internal/cx.js";
import { Button } from "../primitives/button.js";
/** Product-neutral form lifecycle/chrome used inside editor dialogs. */
export function EditorForm({ onSubmit, onCancel, error, errorPrefix, pending, cancelLabel, submitLabel, submitIcon, submitDisabled, children, assistant, classes, }) {
    const form = (_jsxs("form", { className: classes?.form, noValidate: true, onSubmit: onSubmit, children: [children, error ? (_jsx("div", { className: "ui-state ui-state--danger", role: "alert", children: _jsxs("p", { children: [errorPrefix ? _jsxs(_Fragment, { children: [errorPrefix, " "] }) : null, error] }) })) : null, _jsxs("div", { className: classes?.actions, children: [_jsx(Button, { type: "button", variant: "ghost", onClick: onCancel, disabled: pending, children: cancelLabel }), _jsx(Button, { type: "submit", variant: "primary", loading: pending, disabled: submitDisabled, ...(submitIcon ? { icon: submitIcon } : {}), children: submitLabel })] })] }));
    if (!assistant)
        return form;
    return (_jsxs("div", { className: cx("ui-editor-form-split", classes?.split), children: [_jsx("div", { className: cx("ui-editor-form-split__form", classes?.formPane), children: form }), _jsx("div", { className: cx("ui-editor-form-split__assistant", classes?.assistantPane), children: assistant })] }));
}
//# sourceMappingURL=editor-form.js.map