// A data-driven multi-select: a checkbox list of options (id + optional
// description) toggling membership in a string[]. Used for the editor's tools and
// plugins pickers, fed from the capability snapshot.

import { cx } from "./cx";

export interface CheckOption {
  id: string;
  label?: string;
  description?: string;
}

export interface CheckPickerProps {
  options: CheckOption[];
  selected: string[];
  onChange: (next: string[]) => void;
  empty?: string;
  className?: string;
}

export function CheckPicker({ options, selected, onChange, empty, className }: CheckPickerProps) {
  const toggle = (id: string) =>
    onChange(selected.includes(id) ? selected.filter((x) => x !== id) : [...selected, id]);
  if (options.length === 0) return <span className="mut">{empty ?? "—"}</span>;
  return (
    <div className={cx("check-picker", className)}>
      {options.map((o) => (
        <label key={o.id} className="check-row">
          <input type="checkbox" checked={selected.includes(o.id)} onChange={() => toggle(o.id)} />
          <span className="mono">{o.label ?? o.id}</span>
          {o.description && <span className="mut check-desc">{o.description}</span>}
        </label>
      ))}
    </div>
  );
}
