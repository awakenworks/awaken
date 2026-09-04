import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, } from "react";
const ToastContext = createContext(null);
const NOOP_TOAST_API = { push: () => 0, dismiss: () => undefined };
export function useToast({ optional = false } = {}) {
    const api = useContext(ToastContext);
    if (api)
        return api;
    if (optional)
        return NOOP_TOAST_API;
    throw new Error("useToast must be used within ToastProvider");
}
export function ToastProvider({ children, defaultDuration = 4_000, errorDuration = 7_000, dismissLabel, regionLabel, renderIcon, }) {
    const sequence = useRef(0);
    const [toasts, setToasts] = useState([]);
    const dismiss = useCallback((id) => {
        setToasts((current) => current.filter((toast) => toast.id !== id));
    }, []);
    const push = useCallback((request) => {
        const id = ++sequence.current;
        const tone = request.tone ?? "info";
        const duration = request.duration ?? (tone === "danger" || tone === "error" ? errorDuration : defaultDuration);
        setToasts((current) => [...current, { ...request, id, tone, duration }]);
        return id;
    }, [defaultDuration, errorDuration]);
    const api = useMemo(() => ({ dismiss, push }), [dismiss, push]);
    return (_jsxs(ToastContext.Provider, { value: api, children: [children, _jsx("div", { "aria-label": regionLabel, "aria-live": "polite", "aria-atomic": "false", className: "ui-toast-region", role: "region", children: toasts.map((toast) => (_jsx(ToastItem, { toast: toast, dismissLabel: dismissLabel, onDismiss: dismiss, icon: renderIcon?.(toast.tone ?? "info") }, toast.id))) })] }));
}
function ToastItem({ toast, dismissLabel, onDismiss, icon, }) {
    const [paused, setPaused] = useState(false);
    useEffect(() => {
        if (toast.duration <= 0 || paused)
            return;
        const timer = globalThis.setTimeout(() => onDismiss(toast.id), toast.duration);
        return () => globalThis.clearTimeout(timer);
    }, [onDismiss, paused, toast.duration, toast.id]);
    const tone = toast.tone ?? "info";
    return (_jsxs("div", { "aria-atomic": "true", className: "ui-toast", "data-tone": tone, role: tone === "danger" || tone === "error" ? "alert" : "status", onMouseEnter: () => setPaused(true), onMouseLeave: () => setPaused(false), onFocusCapture: () => setPaused(true), onBlurCapture: () => setPaused(false), children: [icon === undefined ? null : _jsx("span", { className: "ui-toast__icon", "aria-hidden": "true", children: icon }), _jsx("div", { className: "ui-toast__message", children: toast.message }), toast.action ? (_jsx("button", { className: "ui-toast__action", type: "button", onClick: () => {
                    toast.action?.onClick();
                    onDismiss(toast.id);
                }, children: toast.action.label })) : null, _jsx("button", { "aria-label": dismissLabel, className: "ui-toast__dismiss", onClick: () => onDismiss(toast.id), type: "button", children: _jsx("span", { "aria-hidden": "true", children: "\u00D7" }) })] }));
}
