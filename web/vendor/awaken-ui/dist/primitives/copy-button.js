import { jsx as _jsx } from "react/jsx-runtime";
import { useEffect, useRef, useState } from "react";
import { cx } from "../internal/cx.js";
import { Button } from "./button.js";
export function CopyButton({ value, label, copiedLabel, icon, copiedIcon, resetAfter = 1500, className, }) {
    const [copied, setCopied] = useState(false);
    const timer = useRef(undefined);
    useEffect(() => () => {
        if (timer.current)
            clearTimeout(timer.current);
    }, []);
    const copy = async () => {
        try {
            if (!globalThis.navigator?.clipboard)
                return;
            await globalThis.navigator.clipboard.writeText(value);
            setCopied(true);
            if (timer.current)
                clearTimeout(timer.current);
            timer.current = setTimeout(() => setCopied(false), resetAfter);
        }
        catch {
            // Clipboard may be blocked; an idle button remains the safe fallback.
        }
    };
    const currentLabel = copied ? copiedLabel : label;
    return (_jsx(Button, { type: "button", variant: "icon", className: cx("ui-copy", copied && "is-copied", className), "aria-label": currentLabel, title: currentLabel, onClick: () => void copy(), children: copied ? copiedIcon ?? _jsx("span", { "aria-hidden": "true", children: "\u2713" }) : icon ?? _jsx("span", { "aria-hidden": "true", children: "\u25A1" }) }));
}
