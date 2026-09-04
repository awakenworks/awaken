import { jsx as _jsx } from "react/jsx-runtime";
import { cx } from "../internal/cx.js";
export function Card({ className, ...props }) {
    return _jsx("section", { ...props, className: cx("ui-card", className) });
}
export function CardHeader({ className, ...props }) {
    return _jsx("div", { ...props, className: cx("ui-card__header", className) });
}
export function CardBody({ className, ...props }) {
    return _jsx("div", { ...props, className: cx("ui-card__body", className) });
}
export function CardFooter({ className, ...props }) {
    return _jsx("div", { ...props, className: cx("ui-card__footer", className) });
}
