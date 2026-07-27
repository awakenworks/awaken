import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
import { Button } from "../primitives/button.js";
/** Product-neutral human-in-the-loop decision embedded in a transcript. */
export function ChatApproval({ title, description, note, onNoteChange, noteLabel, notePlaceholder, approveLabel, rejectLabel, onApprove, onReject, pending = false, }) {
    return (_jsxs("section", { className: "ui-chat-approval", children: [_jsx("strong", { children: title }), description === undefined ? null : _jsx("div", { children: description }), note !== undefined && onNoteChange && noteLabel ? (_jsxs("label", { children: [_jsx("span", { children: noteLabel }), _jsx("textarea", { value: note, placeholder: notePlaceholder, disabled: pending, onChange: (event) => onNoteChange(event.target.value) })] })) : null, _jsxs("div", { className: "ui-chat-approval__actions", children: [_jsx(Button, { variant: "ghost", disabled: pending, onClick: onReject, children: rejectLabel }), _jsx(Button, { variant: "primary", loading: pending, onClick: onApprove, children: approveLabel })] })] }));
}
//# sourceMappingURL=approval.js.map