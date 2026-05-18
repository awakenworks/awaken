import type { ReactNode } from "react";

/* DataGrid — universal admin table primitive.
 * Spec source: awaken-design/_spec-shell.css `.gtbl` + data-grid.html
 *   thead: uppercase eyebrow (10px mono, --brand-text-faint, --bg-soft strip)
 *   tbody: 9/12 padding, hover --bg-soft, hairline borders
 *   density: default 36px row · comfortable 44px (toggle prop)
 *
 * Columns are declarative: each declares `key`, `header`, optional
 * `accessor` (cell renderer), and optional `align` / `width`. The grid
 * tokens `--grid-row-h` / `--grid-head-h` from brand-awaken control sizing.
 *
 * Not included (intentionally — to be added when there's a real consumer):
 *   - row selection / checkbox column
 *   - inline edit (dblclick)
 *   - column resize persistence
 *   - virtual scroll (>200 rows)
 * The 6 admin list pages can migrate incrementally; until they do, this
 * primitive is the canonical pattern for new tables. */

export type DataGridDensity = "default" | "comfortable";

export interface DataGridColumn<T> {
  /** Stable id, used as React key. */
  key: string;
  /** Header label (uppercase eyebrow rendered automatically). */
  header: ReactNode;
  /** Cell renderer; default reads row[key as keyof T] verbatim. */
  accessor?: (row: T) => ReactNode;
  /** Optional column width (e.g. "200px" or "12rem"); default auto. */
  width?: string;
  /** Optional text alignment. */
  align?: "left" | "right" | "center";
}

export function DataGrid<T>({
  columns,
  rows,
  rowKey,
  density = "default",
  empty,
  onRowClick,
  className = "",
}: {
  columns: DataGridColumn<T>[];
  rows: T[];
  /** Extract a stable React key from each row. */
  rowKey: (row: T) => string;
  density?: DataGridDensity;
  /** Optional empty-state node (renders inside an empty tbody). */
  empty?: ReactNode;
  onRowClick?: (row: T) => void;
  className?: string;
}) {
  const rowH =
    density === "comfortable"
      ? "var(--grid-row-h-comfortable, 44px)"
      : "var(--grid-row-h, 36px)";
  const headH = "var(--grid-head-h, 36px)";

  return (
    <div
      className={`overflow-auto rounded-sm border border-line bg-surface ${className}`.trim()}
    >
      <table className="w-full border-collapse text-[13px]">
        <thead>
          <tr>
            {columns.map((col) => (
              <th
                key={col.key}
                scope="col"
                className="border-b border-line bg-soft px-3 text-left font-mono text-[10px] font-semibold uppercase tracking-eyebrow text-fg-faint"
                style={{
                  height: headH,
                  width: col.width,
                  textAlign: col.align ?? "left",
                }}
              >
                {col.header}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.length === 0 ? (
            <tr>
              <td
                colSpan={columns.length}
                className="px-3 py-8 text-center text-sm text-fg-soft"
              >
                {empty ?? "No rows."}
              </td>
            </tr>
          ) : (
            rows.map((row) => (
              <tr
                key={rowKey(row)}
                onClick={onRowClick ? () => onRowClick(row) : undefined}
                className={[
                  "border-b border-line last:border-b-0 hover:bg-soft",
                  onRowClick ? "cursor-pointer" : "",
                ]
                  .join(" ")
                  .trim()}
                style={{ height: rowH }}
              >
                {columns.map((col) => (
                  <td
                    key={col.key}
                    className="px-3 text-fg"
                    style={{ textAlign: col.align ?? "left" }}
                  >
                    {col.accessor
                      ? col.accessor(row)
                      : (row as unknown as Record<string, ReactNode>)[col.key]}
                  </td>
                ))}
              </tr>
            ))
          )}
        </tbody>
      </table>
    </div>
  );
}
