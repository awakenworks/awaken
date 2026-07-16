// A read-only state-transition diagram for one machine (the "A" half of the state-machine
// editor). Auto-lays-out states into columns by longest-path from the initial state and
// draws each transition as a labeled arrow — the trigger tool, plus a 📢 badge when it
// emits a reminder and a ✕/⚠ badge when it denies/warns. A self-transition draws as a loop.
// No graph library: hand-rolled SVG, sized for the small machines agents actually use.

import { type SmMachine, fromList, machineStates } from "./state-machine-presets";

const COL_W = 168;
const ROW_H = 70;
const NODE_W = 104;
const NODE_H = 34;
const PAD = 24;

/** Shorten a tool pattern to its head, e.g. `Read(file_path ~ "*")` → `Read`, `*` → `any`. */
function triggerLabel(on: string): string {
  if (on.trim() === "*") return "any";
  const head = on.split("(")[0].trim();
  return head || on;
}

/** Column per state = BFS shortest distance from `initial` (compact; back-edges don't
 * inflate columns). States unreachable from initial fall to the last column. */
function columns(m: SmMachine, states: string[]): Map<string, number> {
  const adj = new Map<string, string[]>();
  for (const t of m.transitions) {
    for (const f of fromList(t.from)) {
      if (f !== t.to) (adj.get(f) ?? adj.set(f, []).get(f)!).push(t.to);
    }
  }
  const col = new Map<string, number>();
  const queue: string[] = [m.initial];
  col.set(m.initial, 0);
  while (queue.length) {
    const u = queue.shift()!;
    for (const v of adj.get(u) ?? []) {
      if (!col.has(v)) {
        col.set(v, (col.get(u) ?? 0) + 1);
        queue.push(v);
      }
    }
  }
  const maxReached = Math.max(0, ...col.values());
  for (const s of states) if (!col.has(s)) col.set(s, maxReached + 1);
  return col;
}

export default function StateDiagram({ machine }: { machine: SmMachine }) {
  const states = machineStates(machine);
  const col = columns(machine, states);
  const terminal = new Set(machine.terminal ?? []);

  // Stack states within each column.
  const byCol = new Map<number, string[]>();
  for (const s of states) {
    const c = col.get(s) ?? 0;
    (byCol.get(c) ?? byCol.set(c, []).get(c)!).push(s);
  }
  const pos = new Map<string, { x: number; y: number }>();
  for (const [c, list] of byCol) {
    list.forEach((s, i) => pos.set(s, { x: PAD + c * COL_W, y: PAD + i * ROW_H }));
  }
  const maxCol = Math.max(0, ...[...byCol.keys()]);
  const maxRows = Math.max(1, ...[...byCol.values()].map((l) => l.length));
  const width = PAD * 2 + maxCol * COL_W + NODE_W;
  const height = PAD * 2 + (maxRows - 1) * ROW_H + NODE_H + 26;

  const center = (s: string) => {
    const p = pos.get(s)!;
    return { x: p.x + NODE_W / 2, y: p.y + NODE_H / 2 };
  };

  return (
    <svg width="100%" viewBox={`0 0 ${width} ${height}`} style={{ maxHeight: 260, fontFamily: "inherit" }}>
      <defs>
        <marker id="sm-arrow" markerWidth="8" markerHeight="8" refX="7" refY="3" orient="auto">
          <path d="M0,0 L7,3 L0,6 Z" fill="var(--fg3)" />
        </marker>
        <marker id="sm-arrow-deny" markerWidth="8" markerHeight="8" refX="7" refY="3" orient="auto">
          <path d="M0,0 L7,3 L0,6 Z" fill="var(--danger)" />
        </marker>
      </defs>

      {/* Edges first (under nodes). */}
      {machine.transitions.flatMap((t, ti) =>
        fromList(t.from).map((f, fi) => {
          const deny = t.on_violation?.action === "deny";
          const warn = t.on_violation?.action === "warn";
          const stroke = deny ? "var(--danger)" : warn ? "var(--warn)" : "var(--fg3)";
          const marker = deny ? "url(#sm-arrow-deny)" : "url(#sm-arrow)";
          const a = center(f);
          const b = center(t.to);
          const badge = `${t.emit ? "📢" : ""}${deny ? " ✕" : warn ? " ⚠" : ""}`.trim();
          const key = `${ti}-${fi}`;
          if (f === t.to) {
            // Self-loop: an arc above the node.
            const p = pos.get(f)!;
            const cx = p.x + NODE_W / 2;
            const top = p.y;
            return (
              <g key={key}>
                <path
                  d={`M ${cx - 16} ${top} C ${cx - 28} ${top - 34}, ${cx + 28} ${top - 34}, ${cx + 16} ${top}`}
                  fill="none"
                  stroke={stroke}
                  strokeWidth={1.4}
                  markerEnd={marker}
                  strokeDasharray={deny || warn ? "4 3" : undefined}
                />
                <text x={cx} y={top - 30} textAnchor="middle" fontSize={10.5} fill="var(--fg2)">
                  {triggerLabel(t.on)} {badge}
                </text>
              </g>
            );
          }
          const backward = b.x <= a.x;
          const midx = (a.x + b.x) / 2;
          // Forward edges arc/label above; backward edges bow below — so a forward and a
          // back edge between the same pair don't stack their labels on top of each other.
          const bow = backward ? 26 : 0;
          const endX = backward ? b.x + NODE_W / 2 + 4 : b.x - NODE_W / 2 - 4;
          const ctrlY = (a.y + b.y) / 2 + bow;
          const midy = ctrlY + (backward ? 16 : -8);
          return (
            <g key={key}>
              <path
                d={`M ${a.x} ${a.y} C ${midx} ${ctrlY}, ${midx} ${ctrlY}, ${endX} ${b.y}`}
                fill="none"
                stroke={stroke}
                strokeWidth={1.4}
                markerEnd={marker}
                strokeDasharray={deny || warn ? "4 3" : undefined}
              />
              <text x={midx} y={midy} textAnchor="middle" fontSize={10.5} fill="var(--fg2)">
                {triggerLabel(t.on)} {badge}
              </text>
            </g>
          );
        }),
      )}

      {/* Nodes. */}
      {states.map((s) => {
        const p = pos.get(s)!;
        const isInitial = s === machine.initial;
        const isTerminal = terminal.has(s);
        return (
          <g key={s}>
            {isInitial && (
              <path d={`M ${p.x - 16} ${p.y + NODE_H / 2} L ${p.x - 4} ${p.y + NODE_H / 2}`} stroke="var(--agent)" strokeWidth={1.6} markerEnd="url(#sm-arrow)" />
            )}
            <rect
              x={p.x}
              y={p.y}
              width={NODE_W}
              height={NODE_H}
              rx={8}
              fill="var(--soft)"
              stroke={isInitial ? "var(--agent)" : "var(--line)"}
              strokeWidth={isInitial ? 1.6 : 1}
            />
            {isTerminal && <rect x={p.x + 2.5} y={p.y + 2.5} width={NODE_W - 5} height={NODE_H - 5} rx={6} fill="none" stroke="var(--ok)" strokeWidth={1} />}
            <text x={p.x + NODE_W / 2} y={p.y + NODE_H / 2 + 4} textAnchor="middle" fontSize={12} fill="var(--fg)">
              {s}
            </text>
          </g>
        );
      })}
    </svg>
  );
}
