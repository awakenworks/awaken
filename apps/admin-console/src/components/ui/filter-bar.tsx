import type { ReactNode } from "react";
import { Eyebrow } from "./eyebrow";

export function FilterBar({
  filters,
  sort,
  meta,
}: {
  filters?: ReactNode;
  sort?: ReactNode;
  meta?: ReactNode;
}) {
  return (
    <div className="mb-3 flex flex-wrap items-center gap-3 rounded-md border border-line bg-soft px-3 py-2">
      {filters !== undefined && (
        <div className="flex flex-wrap items-center gap-2">
          <Eyebrow>Filter</Eyebrow>
          {filters}
        </div>
      )}
      {sort !== undefined && (
        <div className="flex items-center gap-2">
          <Eyebrow>Sort</Eyebrow>
          {sort}
        </div>
      )}
      {meta !== undefined && (
        <div className="ml-auto font-mono text-xs text-fg-faint">{meta}</div>
      )}
    </div>
  );
}

export interface FilterChipOption<V extends string = string> {
  value: V;
  label: string;
}

export function FilterChip<V extends string>({
  label,
  value,
  options,
  onChange,
}: {
  label: string;
  value: V;
  options: FilterChipOption<V>[];
  onChange: (next: V) => void;
}) {
  const current = options.find((o) => o.value === value);
  return (
    <label className="inline-flex cursor-pointer items-center gap-1.5 rounded-pill border border-line-strong bg-surface px-2.5 py-1 text-xs text-fg-soft transition-colors hover:bg-soft hover:text-fg">
      <span className="text-fg-faint">{label}</span>
      <span aria-hidden>·</span>
      <select
        className="cursor-pointer appearance-none border-0 bg-transparent text-xs font-medium text-fg outline-none"
        value={value}
        onChange={(e) => onChange(e.target.value as V)}
        aria-label={label}
      >
        {options.map((opt) => (
          <option key={opt.value} value={opt.value}>
            {opt.label}
          </option>
        ))}
      </select>
      <span aria-hidden className="text-fg-faint">▾</span>
      <span className="sr-only">: {current?.label}</span>
    </label>
  );
}
