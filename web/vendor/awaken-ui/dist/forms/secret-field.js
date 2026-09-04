import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
import { useState } from "react";
import { cx } from "../internal/cx.js";
import { SegmentedControl } from "./segmented-control.js";
/** Write-only keep/replace/clear editor; stored secret values never enter props. */
export function SecretField({ label, hasStored, onChange, labels, placeholder, classes, }) {
    const [mode, setMode] = useState(hasStored ? "keep" : "replace");
    const [value, setValue] = useState("");
    const pick = (nextMode) => {
        setMode(nextMode);
        onChange(nextMode === "replace"
            ? { mode: "replace", value }
            : { mode: nextMode });
    };
    const options = (hasStored
        ? ["keep", "replace", "clear"]
        : ["replace"]);
    return (_jsxs("div", { className: cx("ui-secret-field", classes?.root), children: [_jsx("label", { className: classes?.label, children: label }), hasStored ? (_jsx(SegmentedControl, { onChange: pick, options: options.map((value) => ({ value, label: labels[value] })), value: mode, ...(classes?.activeMode ? { activeClassName: classes.activeMode } : {}), ...(classes?.modeButton ? { buttonClassName: classes.modeButton } : {}), ...(classes?.modes ? { className: classes.modes } : {}), ...(classes?.inactiveMode ? { inactiveClassName: classes.inactiveMode } : {}) })) : null, mode === "replace" ? (_jsx("input", { autoComplete: "off", className: cx("ui-secret-field__input", classes?.input), onChange: (event) => {
                    setValue(event.target.value);
                    onChange({ mode: "replace", value: event.target.value });
                }, placeholder: placeholder ?? labels.placeholder, type: "password", value: value })) : (_jsx("span", { className: cx("ui-secret-field__status", classes?.status), children: mode === "keep" ? labels.kept : labels.cleared }))] }));
}
