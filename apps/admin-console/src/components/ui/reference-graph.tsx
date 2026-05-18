import { useId } from "react";

export interface GraphNode {
  id: string;
  label: string;
  /** Smaller subtitle line under the label (e.g. provider id). */
  sub?: string;
  /** Tone tints the node card border. */
  tone?: "neutral" | "agent" | "info" | "warn";
}

export interface GraphColumn {
  id: string;
  label: string;
  nodes: GraphNode[];
}

export interface GraphEdge {
  /** Source node id (must exist somewhere in `columns`). */
  from: string;
  /** Target node id. */
  to: string;
}

const TONE_BORDER: Record<NonNullable<GraphNode["tone"]>, string> = {
  neutral: "border-line",
  agent: "border-agent-stripe/40",
  info: "border-tone-info/40",
  warn: "border-tone-warn/40",
};

const NODE_W = 200;
const NODE_H = 56;
const COL_GAP = 96;
const ROW_GAP = 14;
const PAD_Y = 16;

/**
 * Three-column dependency graph (agents → models → providers, etc.).
 * Renders nodes as HTML overlays absolutely positioned over an SVG that
 * draws the bezier edges. Pixel positions are computed deterministically
 * from input order so the layout stays stable across renders.
 */
export function ReferenceGraph({
  columns,
  edges,
  ariaLabel = "reference graph",
}: {
  columns: GraphColumn[];
  edges: GraphEdge[];
  ariaLabel?: string;
}) {
  const titleId = useId();
  if (columns.length === 0) {
    return (
      <div className="rounded-sm border border-dashed border-line bg-canvas px-6 py-12 text-center text-sm text-fg-soft">
        No nodes to graph yet.
      </div>
    );
  }

  // Compute (x, y) per node id.
  const positions = new Map<string, { x: number; y: number; col: number }>();
  columns.forEach((col, ci) => {
    col.nodes.forEach((node, ni) => {
      positions.set(node.id, {
        col: ci,
        x: ci * (NODE_W + COL_GAP),
        y: PAD_Y + ni * (NODE_H + ROW_GAP),
      });
    });
  });
  const maxRows = Math.max(...columns.map((c) => c.nodes.length), 1);
  const width = columns.length * NODE_W + (columns.length - 1) * COL_GAP;
  const height = PAD_Y * 2 + maxRows * NODE_H + (maxRows - 1) * ROW_GAP;

  return (
    <div className="overflow-x-auto">
      <div
        className="relative"
        style={{ width, height }}
        role="figure"
        aria-labelledby={titleId}
      >
        <span id={titleId} className="sr-only">
          {ariaLabel}
        </span>

        {/* Column headers */}
        {columns.map((col, ci) => (
          <div
            key={`hdr-${col.id}`}
            className="absolute text-[11px] font-medium uppercase tracking-[0.18em] text-fg-faint"
            style={{ left: ci * (NODE_W + COL_GAP), top: -2, width: NODE_W }}
          >
            {col.label}
          </div>
        ))}

        {/* SVG edges layer */}
        <svg
          aria-hidden
          className="absolute inset-0"
          width={width}
          height={height}
        >
          {edges.map((edge, idx) => {
            const a = positions.get(edge.from);
            const b = positions.get(edge.to);
            if (!a || !b) return null;
            const x1 = a.x + NODE_W;
            const y1 = a.y + NODE_H / 2;
            const x2 = b.x;
            const y2 = b.y + NODE_H / 2;
            const dx = (x2 - x1) / 2;
            const path = `M${x1} ${y1} C${x1 + dx} ${y1}, ${x2 - dx} ${y2}, ${x2} ${y2}`;
            return (
              <path
                key={`e-${idx}`}
                d={path}
                fill="none"
                stroke="var(--aw-text-soft)"
                strokeOpacity={0.55}
                strokeWidth={1.5}
              />
            );
          })}
        </svg>

        {/* Node cards */}
        {columns.flatMap((col) =>
          col.nodes.map((node) => {
            const pos = positions.get(node.id)!;
            return (
              <div
                key={node.id}
                className={[
                  "absolute flex flex-col justify-center rounded-sm border bg-surface px-3 py-2 shadow-card",
                  TONE_BORDER[node.tone ?? "neutral"],
                ].join(" ")}
                style={{ left: pos.x, top: pos.y, width: NODE_W, height: NODE_H }}
              >
                <div className="truncate font-mono text-xs font-medium text-fg-strong">
                  {node.label}
                </div>
                {node.sub && (
                  <div className="truncate text-[11px] text-fg-soft">{node.sub}</div>
                )}
              </div>
            );
          }),
        )}
      </div>
    </div>
  );
}
