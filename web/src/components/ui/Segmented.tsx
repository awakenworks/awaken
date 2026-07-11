// Segmented control: a generic value-toggle rendered as a row of buttons (the
// All/Running/Archived filter pattern). Generic over the option value so a
// call site keeps a typed union.

import { cx } from "./cx";

export interface SegmentedOption<T extends string> {
  value: T;
  label: string;
}

export interface SegmentedProps<T extends string> {
  options: SegmentedOption<T>[];
  value: T;
  onChange: (value: T) => void;
  className?: string;
}

export function Segmented<T extends string>({ options, value, onChange, className }: SegmentedProps<T>) {
  return (
    <span className={cx("row", className)}>
      {options.map((o) => (
        <button
          key={o.value}
          className={cx("btn", value === o.value && "primary")}
          style={{ height: 26 }}
          onClick={() => onChange(o.value)}
        >
          {o.label}
        </button>
      ))}
    </span>
  );
}
