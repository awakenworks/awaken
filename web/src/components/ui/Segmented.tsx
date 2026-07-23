import { SegmentedControl } from "@awaken/ui";
import { cx } from "./cx";

export interface SegmentedOption<T extends string> {
  readonly value: T;
  readonly label: string;
}

export interface SegmentedProps<T extends string> {
  readonly options: ReadonlyArray<SegmentedOption<T>>;
  readonly value: T;
  readonly onChange: (value: T) => void;
  readonly className?: string;
}

export function Segmented<T extends string>({ options, value, onChange, className }: SegmentedProps<T>) {
  return (
    <SegmentedControl
      as="span"
      options={options}
      value={value}
      onChange={onChange}
      className={cx("row", className)}
      buttonClassName="btn"
      activeClassName="primary"
      buttonStyle={{ height: 26 }}
    />
  );
}
