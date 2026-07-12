// A sortable, paginated, filterable table driven by URL-persisted list state
// (useListState). Columns declare an optional sortValue; the header toggles sort,
// a search box filters client-side, and a pager slices to pageSize.

import type { ReactNode } from "react";
import { useApp } from "../../lib/app-state";
import type { ListState } from "../../lib/useListState";
import { EmptyState, SkeletonRows } from "./primitives";

export interface Column<T> {
  key: string;
  header: ReactNode;
  cell: (row: T) => ReactNode;
  /** Provide to make the column sortable. */
  sortValue?: (row: T) => string | number;
  align?: "left" | "right";
  width?: number | string;
}

export interface DataGridProps<T> {
  rows: T[];
  columns: Column<T>[];
  rowKey: (row: T) => string;
  state: ListState;
  /** Client-side text filter against `state.q`. */
  filter?: (row: T, q: string) => boolean;
  onRowClick?: (row: T) => void;
  pageSize?: number;
  loading?: boolean;
  emptyTitle?: string;
  emptyHint?: string;
  /** Extra controls (filter chips) rendered next to the search box. */
  toolbar?: ReactNode;
  searchPlaceholder?: string;
}

export function DataGrid<T>({
  rows,
  columns,
  rowKey,
  state,
  filter,
  onRowClick,
  pageSize = 20,
  loading = false,
  emptyTitle,
  emptyHint,
  toolbar,
  searchPlaceholder,
}: DataGridProps<T>) {
  const app = useApp();

  const filtered = filter && state.q ? rows.filter((r) => filter(r, state.q)) : rows;

  const col = columns.find((c) => c.key === state.sort && c.sortValue);
  const sorted = col
    ? [...filtered].sort((a, b) => {
        const av = col.sortValue!(a);
        const bv = col.sortValue!(b);
        const cmp = typeof av === "number" && typeof bv === "number" ? av - bv : String(av).localeCompare(String(bv));
        return state.dir === "asc" ? cmp : -cmp;
      })
    : filtered;

  const total = sorted.length;
  const pages = Math.max(1, Math.ceil(total / pageSize));
  const page = Math.min(state.page, pages);
  const slice = sorted.slice((page - 1) * pageSize, page * pageSize);

  return (
    <>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <div className="row">
          {filter && (
            <input
              className="input"
              style={{ width: 240 }}
              placeholder={searchPlaceholder ?? app.t("Filter…", "过滤…")}
              value={state.q}
              onChange={(e) => state.setQ(e.target.value)}
            />
          )}
          {toolbar}
        </div>
        <span className="mut">
          {total === 0
            ? ""
            : app.t(
                `${(page - 1) * pageSize + 1}–${Math.min(page * pageSize, total)} of ${total}`,
                `${(page - 1) * pageSize + 1}–${Math.min(page * pageSize, total)} / ${total}`,
              )}
        </span>
      </div>

      <div className="card grid-scroll" style={{ padding: 0 }}>
        <table className="table">
          <thead>
            <tr>
              {columns.map((c) => (
                <th
                  key={c.key}
                  style={{ textAlign: c.align ?? "left", width: c.width, cursor: c.sortValue ? "pointer" : "default" }}
                  onClick={() => c.sortValue && state.setSort(c.key)}
                >
                  {c.header}
                  {c.sortValue && state.sort === c.key && <span className="mut"> {state.dir === "asc" ? "▲" : "▼"}</span>}
                </th>
              ))}
            </tr>
          </thead>
          {loading ? (
            <SkeletonRows rows={4} cols={columns.length} />
          ) : (
            <tbody>
              {slice.map((r) => (
                <tr
                  key={rowKey(r)}
                  data-click={onRowClick ? "true" : undefined}
                  onClick={onRowClick ? () => onRowClick(r) : undefined}
                >
                  {columns.map((c) => (
                    <td key={c.key} style={{ textAlign: c.align ?? "left" }}>
                      {c.cell(r)}
                    </td>
                  ))}
                </tr>
              ))}
              {slice.length === 0 && (
                <tr>
                  <td colSpan={columns.length} style={{ padding: 0 }}>
                    <EmptyState title={emptyTitle ?? app.t("Nothing here yet.", "暂无数据。")} hint={emptyHint} />
                  </td>
                </tr>
              )}
            </tbody>
          )}
        </table>
      </div>

      {pages > 1 && (
        <div className="row" style={{ justifyContent: "flex-end" }}>
          <button className="btn ghost" style={{ height: 26 }} disabled={page <= 1} onClick={() => state.setPage(page - 1)}>
            ← {app.t("Prev", "上一页")}
          </button>
          <span className="mut">
            {page} / {pages}
          </span>
          <button className="btn ghost" style={{ height: 26 }} disabled={page >= pages} onClick={() => state.setPage(page + 1)}>
            {app.t("Next", "下一页")} →
          </button>
        </div>
      )}
    </>
  );
}
