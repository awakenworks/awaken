export function SkeletonBlock({
  className = "",
  width = "100%",
  height = "12px",
}: {
  className?: string;
  width?: string | number;
  height?: string | number;
}) {
  return (
    <span
      aria-hidden
      className={`inline-block animate-pulse rounded-sm bg-muted ${className}`.trim()}
      style={{ width, height }}
    />
  );
}

/** Renders N <tr> rows with shimmering placeholder cells. */
export function SkeletonRows({
  rows = 3,
  cols = 4,
}: {
  rows?: number;
  cols?: number;
}) {
  return (
    <>
      {Array.from({ length: rows }).map((_, rowIdx) => (
        <tr key={`skel-${rowIdx}`} className="border-t border-line">
          {Array.from({ length: cols }).map((_, colIdx) => (
            <td key={colIdx} className="px-5 py-4">
              <SkeletonBlock
                width={colIdx === 0 ? "60%" : colIdx === cols - 1 ? "30%" : "75%"}
              />
            </td>
          ))}
        </tr>
      ))}
    </>
  );
}
