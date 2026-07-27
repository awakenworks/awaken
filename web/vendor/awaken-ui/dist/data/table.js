import { jsx as _jsx } from "react/jsx-runtime";
import { cx } from "../internal/cx.js";
export function DataTable({ className, ...props }) {
    return _jsx("div", { ...props, className: cx("ui-table", className), role: "table" });
}
export function TableHead({ className, ...props }) {
    return _jsx("div", { ...props, className: cx("ui-table__head", className), role: "rowgroup" });
}
export function TableBody({ className, ...props }) {
    return _jsx("div", { ...props, className: cx("ui-table__body", className), role: "rowgroup" });
}
export function TableRow({ className, ...props }) {
    return _jsx("div", { ...props, className: cx("ui-table__row", className), role: "row" });
}
export function TableHeaderCell({ className, ...props }) {
    return _jsx("span", { ...props, className: cx("ui-table__th", className), role: "columnheader" });
}
export function TableCell({ className, ...props }) {
    return _jsx("span", { ...props, className: cx("ui-table__td", className), role: "cell" });
}
//# sourceMappingURL=table.js.map