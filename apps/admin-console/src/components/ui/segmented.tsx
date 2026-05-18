import type { ReactNode } from "react";

export interface SegmentedOption<V extends string> {
  value: V;
  label: ReactNode;
}

/**
 * Segmented radio control. Each option has a leading dot that fills when the
 * option is active, matching the design's `.w2b-segmented` pattern. ARIA
 * `radiogroup` semantics so screen readers announce the selection state.
 */
export function SegmentedControl<V extends string>({
  value,
  onChange,
  options,
  ariaLabel,
  className = "",
}: {
  value: V;
  onChange: (next: V) => void;
  options: SegmentedOption<V>[];
  ariaLabel?: string;
  className?: string;
}) {
  return (
    <div
      role="radiogroup"
      aria-label={ariaLabel}
      className={`inline-flex items-center gap-1 rounded-sm border border-line bg-soft p-0.5 ${className}`.trim()}
    >
      {options.map((opt) => {
        const active = opt.value === value;
        return (
          <button
            key={opt.value}
            type="button"
            role="radio"
            aria-checked={active}
            onClick={() => onChange(opt.value)}
            className={[
              "inline-flex h-7 items-center gap-1.5 rounded-sm px-2.5 text-xs font-medium transition-colors",
              active
                ? "bg-surface text-fg-strong shadow-card"
                : "text-fg-soft hover:text-fg",
            ].join(" ")}
          >
            <span
              aria-hidden
              className={[
                "inline-block h-1.5 w-1.5 rounded-pill border",
                active
                  ? "border-accent bg-accent"
                  : "border-line-strong bg-transparent",
              ].join(" ")}
            />
            <span>{opt.label}</span>
          </button>
        );
      })}
    </div>
  );
}
