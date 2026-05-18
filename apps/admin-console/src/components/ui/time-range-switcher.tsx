/**
 * Compact time-range chip group used by Dashboard / observability views.
 * Mirrors the design's `aw-range` strip (15m · 1h · 24h · 7d).
 */
export type TimeRange = "15m" | "1h" | "24h" | "7d" | "30d";

const PRESET_LABELS: Record<TimeRange, string> = {
  "15m": "15m",
  "1h": "1h",
  "24h": "24h",
  "7d": "7d",
  "30d": "30d",
};

export const TIME_RANGE_SECONDS: Record<TimeRange, number> = {
  "15m": 15 * 60,
  "1h": 60 * 60,
  "24h": 24 * 60 * 60,
  "7d": 7 * 24 * 60 * 60,
  "30d": 30 * 24 * 60 * 60,
};

export function TimeRangeSwitcher({
  value,
  onChange,
  options = ["15m", "1h", "24h", "7d"],
  className = "",
}: {
  value: TimeRange;
  onChange: (next: TimeRange) => void;
  options?: TimeRange[];
  className?: string;
}) {
  return (
    <div
      role="radiogroup"
      aria-label="Time range"
      className={`inline-flex items-center rounded-sm border border-line bg-surface p-0.5 text-xs ${className}`.trim()}
    >
      {options.map((opt) => {
        const active = opt === value;
        return (
          <button
            key={opt}
            type="button"
            role="radio"
            aria-checked={active}
            onClick={() => onChange(opt)}
            className={[
              "rounded-sm px-2 py-1 font-medium transition-colors",
              active
                ? "bg-soft text-fg-strong"
                : "text-fg-soft hover:text-fg",
            ].join(" ")}
          >
            {PRESET_LABELS[opt]}
          </button>
        );
      })}
    </div>
  );
}
