import { jsx as _jsx, jsxs as _jsxs, Fragment as _Fragment } from "react/jsx-runtime";
import { cx } from "../internal/cx.js";
export function DataGrid({ rows, columns, rowKey, state, labels, filter, onRowClick, pageSize = 20, loading = false, toolbar, mobileCards = false, mobileRowActionLabel, renderMobileLoading, renderLoading, renderEmpty, classes, }) {
    const filtered = filter && state.q
        ? rows.filter((row) => filter(row, state.q))
        : rows;
    const sortColumn = columns.find((column) => column.key === state.sort && column.sortValue);
    const sorted = sortColumn
        ? [...filtered].sort((a, b) => {
            const aValue = sortColumn.sortValue?.(a);
            const bValue = sortColumn.sortValue?.(b);
            const comparison = typeof aValue === "number" && typeof bValue === "number"
                ? aValue - bValue
                : String(aValue).localeCompare(String(bValue));
            return state.dir === "asc" ? comparison : -comparison;
        })
        : filtered;
    const total = sorted.length;
    const pages = Math.max(1, Math.ceil(total / pageSize));
    const page = Math.min(state.page, pages);
    const pageRows = sorted.slice((page - 1) * pageSize, page * pageSize);
    return (_jsxs(_Fragment, { children: [_jsxs("div", { className: cx("ui-data-grid__toolbar", classes?.toolbar), children: [_jsxs("div", { className: cx("ui-data-grid__toolbar-lead", classes?.toolbarLead), children: [filter ? (_jsx("input", { className: cx("ui-data-grid__search", classes?.search), placeholder: labels.searchPlaceholder, value: state.q, onChange: (event) => state.setQ(event.target.value) })) : null, toolbar] }), _jsx("span", { className: cx("ui-data-grid__range", classes?.range), children: total === 0 ? null : labels.formatRange({
                            from: (page - 1) * pageSize + 1,
                            to: Math.min(page * pageSize, total),
                            total,
                        }) })] }), _jsx("div", { className: cx("ui-data-grid__scroll", classes?.scroll), "data-mobile-cards": mobileCards ? "true" : undefined, children: _jsxs("table", { className: classes?.table, children: [_jsx("thead", { children: _jsx("tr", { children: columns.map((column) => (_jsxs("th", { style: {
                                        cursor: column.sortValue ? "pointer" : "default",
                                        textAlign: column.align ?? "left",
                                        width: column.width,
                                    }, onClick: () => {
                                        if (column.sortValue)
                                            state.setSort(column.key);
                                    }, children: [column.header, column.sortValue && state.sort === column.key ? (_jsxs("span", { className: classes?.muted, children: [" ", state.dir === "asc" ? "▲" : "▼"] })) : null] }, column.key))) }) }), loading ? renderLoading(columns.length) : (_jsxs("tbody", { children: [pageRows.map((row) => (_jsx("tr", { "data-click": onRowClick ? "true" : undefined, onClick: onRowClick ? () => onRowClick(row) : undefined, children: columns.map((column) => (_jsx("td", { style: { textAlign: column.align ?? "left" }, children: column.cell(row) }, column.key))) }, rowKey(row)))), pageRows.length === 0 ? (_jsx("tr", { children: _jsx("td", { className: "ui-data-grid__empty", colSpan: columns.length, children: renderEmpty() }) })) : null] }))] }) }), mobileCards ? (_jsx("div", { className: "ui-data-grid__cards", children: loading ? renderMobileLoading?.() : (_jsxs(_Fragment, { children: [pageRows.map((row) => (_jsxs("article", { className: "ui-data-grid__card", children: [_jsx("dl", { className: "ui-data-grid__card-fields", children: columns.map((column) => (_jsxs("div", { className: "ui-data-grid__card-field", children: [_jsx("dt", { children: column.header }), _jsx("dd", { style: { textAlign: column.align ?? "left" }, children: column.cell(row) })] }, column.key))) }), onRowClick && mobileRowActionLabel ? (_jsx("button", { className: classes?.button, onClick: () => onRowClick(row), type: "button", children: typeof mobileRowActionLabel === "function"
                                        ? mobileRowActionLabel(row)
                                        : mobileRowActionLabel })) : null] }, rowKey(row)))), pageRows.length === 0 ? renderEmpty() : null] })) })) : null, pages > 1 ? (_jsxs("div", { className: cx("ui-data-grid__pager", classes?.pager), children: [_jsx("button", { className: classes?.button, disabled: page <= 1, onClick: () => state.setPage(page - 1), type: "button", children: labels.previous }), _jsxs("span", { className: classes?.muted, children: [page, " / ", pages] }), _jsx("button", { className: classes?.button, disabled: page >= pages, onClick: () => state.setPage(page + 1), type: "button", children: labels.next })] })) : null] }));
}
//# sourceMappingURL=grid.js.map