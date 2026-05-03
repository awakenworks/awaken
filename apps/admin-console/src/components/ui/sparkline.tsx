/**
 * Tiny inline trend line for tables / dashboards.
 * Pure SVG, no chart library — intentionally simple.
 *
 * Match the design's `aw-spark-svg` shape: 56×18 px, currentColor stroke,
 * 1.2 px line width, no fill, no axes.
 */
export function Sparkline({
  values,
  width = 56,
  height = 18,
  className = "",
  ariaLabel,
}: {
  values: number[];
  width?: number;
  height?: number;
  className?: string;
  ariaLabel?: string;
}) {
  if (values.length < 2) {
    return (
      <span
        aria-hidden
        className={`inline-block h-px w-14 bg-fg-faint/40 ${className}`.trim()}
      />
    );
  }
  const max = Math.max(...values);
  const min = Math.min(...values);
  const range = max - min;
  const step = width / (values.length - 1);
  const yFor = (v: number): number => {
    if (range === 0) return height / 2; // flat-line in middle when constant
    return height - ((v - min) / range) * height;
  };
  const points = values
    .map((v, i) => `${(i * step).toFixed(2)},${yFor(v).toFixed(2)}`)
    .join(" ");
  return (
    <svg
      role={ariaLabel ? "img" : undefined}
      aria-label={ariaLabel}
      aria-hidden={ariaLabel ? undefined : true}
      width={width}
      height={height}
      viewBox={`0 0 ${width} ${height}`}
      className={`inline-block ${className}`.trim()}
    >
      <polyline
        fill="none"
        stroke="currentColor"
        strokeWidth={1.2}
        strokeLinecap="round"
        strokeLinejoin="round"
        points={points}
      />
    </svg>
  );
}
