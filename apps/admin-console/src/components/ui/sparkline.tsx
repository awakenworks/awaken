/**
 * Tiny inline polyline sparkline. No deps, currentColor-aware so the consumer
 * controls hue via Tailwind text-* classes. Renders a flat line for empty
 * series so the column width stays consistent.
 */
export function Sparkline({
  values,
  width = 56,
  height = 18,
  ariaLabel,
}: {
  values: number[];
  width?: number;
  height?: number;
  ariaLabel?: string;
}) {
  if (values.length === 0) {
    return (
      <svg
        width={width}
        height={height}
        viewBox={`0 0 ${width} ${height}`}
        role="img"
        aria-label={ariaLabel ?? "no activity"}
        className="text-fg-faint"
      >
        <line
          x1={0}
          y1={height / 2}
          x2={width}
          y2={height / 2}
          stroke="currentColor"
          strokeWidth={1}
          strokeDasharray="2 2"
        />
      </svg>
    );
  }
  const max = Math.max(...values, 1);
  const step = values.length === 1 ? 0 : width / (values.length - 1);
  const points = values
    .map((v, i) => `${(i * step).toFixed(2)},${(height - (v / max) * height).toFixed(2)}`)
    .join(" ");
  return (
    <svg
      width={width}
      height={height}
      viewBox={`0 0 ${width} ${height}`}
      role="img"
      aria-label={ariaLabel ?? `sparkline of ${values.length} samples`}
      className="text-link"
    >
      <polyline
        fill="none"
        stroke="currentColor"
        strokeWidth={1.2}
        points={points}
      />
    </svg>
  );
}
