import type { ReactNode } from "react";
import {
  PAGE_SIZE_OPTIONS,
  type PageSize,
  type SortDirection,
  type SortState,
} from "@/lib/list-view";

export function ListSearchBar({
  value,
  onChange,
  placeholder = "Search by id…",
}: {
  value: string;
  onChange: (next: string) => void;
  placeholder?: string;
}) {
  return (
    <label className="relative block w-full max-w-xs">
      <span className="sr-only">Search</span>
      <input
        type="search"
        value={value}
        onChange={(event) => onChange(event.target.value)}
        placeholder={placeholder}
        className="w-full rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
      />
    </label>
  );
}

export function PageSizeSelect({
  value,
  onChange,
}: {
  value: PageSize;
  onChange: (next: PageSize) => void;
}) {
  return (
    <label className="flex items-center gap-2 text-xs text-slate-500">
      <span>Rows per page</span>
      <select
        value={value}
        onChange={(event) => onChange(Number(event.target.value) as PageSize)}
        className="rounded-md border border-slate-300 bg-white px-2 py-1 text-xs text-slate-700 outline-none focus:border-slate-500"
      >
        {PAGE_SIZE_OPTIONS.map((size) => (
          <option key={size} value={size}>
            {size}
          </option>
        ))}
      </select>
    </label>
  );
}

export function Pagination({
  page,
  pageCount,
  startIndex,
  endIndex,
  totalItems,
  onPageChange,
}: {
  page: number;
  pageCount: number;
  startIndex: number;
  endIndex: number;
  totalItems: number;
  onPageChange: (next: number) => void;
}) {
  const disablePrev = page <= 1;
  const disableNext = page >= pageCount;
  return (
    <div className="flex items-center justify-between gap-3 border-t border-slate-200 bg-slate-50 px-4 py-2 text-xs text-slate-500">
      <span>
        {totalItems === 0
          ? "0 results"
          : `${startIndex + 1}–${endIndex} of ${totalItems}`}
      </span>
      <div className="flex items-center gap-2">
        <button
          type="button"
          onClick={() => onPageChange(page - 1)}
          disabled={disablePrev}
          className="rounded-md border border-slate-300 bg-white px-2 py-1 font-medium text-slate-600 transition hover:bg-slate-100 disabled:cursor-not-allowed disabled:opacity-50"
        >
          ‹ Prev
        </button>
        <span className="font-mono text-slate-600">
          {page}/{pageCount}
        </span>
        <button
          type="button"
          onClick={() => onPageChange(page + 1)}
          disabled={disableNext}
          className="rounded-md border border-slate-300 bg-white px-2 py-1 font-medium text-slate-600 transition hover:bg-slate-100 disabled:cursor-not-allowed disabled:opacity-50"
        >
          Next ›
        </button>
      </div>
    </div>
  );
}

export interface SortableColumn<TKey extends string> {
  key: TKey | null;
  label: ReactNode;
  align?: "left" | "right";
  className?: string;
}

export function SortableHeader<TKey extends string>({
  columns,
  sort,
  onSort,
}: {
  columns: SortableColumn<TKey>[];
  sort: SortState<TKey> | null;
  onSort: (key: TKey) => void;
}) {
  return (
    <thead className="bg-slate-50 text-left text-xs uppercase tracking-wide text-slate-500">
      <tr>
        {columns.map((column, idx) => {
          const align = column.align === "right" ? "text-right" : "text-left";
          const baseClass = `px-5 py-3 ${align} ${column.className ?? ""}`.trim();
          if (!column.key) {
            return (
              <th key={`col-${idx}`} className={baseClass}>
                {column.label}
              </th>
            );
          }
          const active = sort?.key === column.key;
          return (
            <th key={column.key} className={baseClass}>
              <button
                type="button"
                onClick={() => onSort(column.key as TKey)}
                className={[
                  "inline-flex items-center gap-1 font-semibold uppercase tracking-wide transition",
                  active ? "text-slate-900" : "text-slate-500 hover:text-slate-700",
                ].join(" ")}
                aria-sort={ariaSort(active, sort?.direction)}
              >
                <span>{column.label}</span>
                <SortIndicator
                  active={active}
                  direction={sort?.direction ?? null}
                />
              </button>
            </th>
          );
        })}
      </tr>
    </thead>
  );
}

function ariaSort(
  active: boolean,
  direction: SortDirection | undefined,
): "ascending" | "descending" | "none" {
  if (!active || !direction) return "none";
  return direction === "asc" ? "ascending" : "descending";
}

function SortIndicator({
  active,
  direction,
}: {
  active: boolean;
  direction: SortDirection | null;
}) {
  if (!active) {
    return (
      <span aria-hidden className="text-slate-300">
        ↕
      </span>
    );
  }
  return (
    <span aria-hidden className="text-slate-700">
      {direction === "asc" ? "▲" : "▼"}
    </span>
  );
}
