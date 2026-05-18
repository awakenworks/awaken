/* PhaseRibbonMini — 9-phase inline status indicator for run/trace rows.
 * Spec source: awaken-design/_spec-shell.css `.ribbon-mini`
 *   9 vertical bars · 9×14px · 3px gap · 2px radius
 *   pending: 22% chroma of --brand-p{n}
 *   done:    100% chroma
 *   active:  100% + 2px outer glow (32% chroma)
 *   error:   --brand-tone-error (one phase failed) */

export type PhaseStatus = "pending" | "done" | "active" | "error";

const PHASE_NAMES = [
  "resolve",
  "prepare",
  "prompt",
  "stream",
  "gate",
  "tool",
  "commit",
  "events",
  "finalize",
] as const;

/* color-mix on inline style keeps each bar tied to its --brand-p{n} token
 * (which itself aliases --aw-phase-*). Single source: changing the brand
 * 9-phase palette flips every consumer. */
const PHASE_VAR = (i: number) => `var(--brand-p${i + 1})`;

function barStyle(state: PhaseStatus, idx: number): React.CSSProperties {
  const c = PHASE_VAR(idx);
  switch (state) {
    case "done":
      return { background: c };
    case "active":
      return {
        background: c,
        boxShadow: `0 0 0 2px color-mix(in oklch, ${c} 32%, transparent)`,
      };
    case "error":
      return { background: "var(--aw-tone-error)" };
    case "pending":
    default:
      return { background: `color-mix(in oklch, ${c} 22%, transparent)` };
  }
}

export function PhaseRibbonMini({
  states,
  className = "",
}: {
  /** Length-9 array describing each phase status. Shorter lists are padded
   *  with "pending" at the tail. */
  states: PhaseStatus[];
  className?: string;
}) {
  const padded: PhaseStatus[] = Array.from(
    { length: 9 },
    (_, i) => states[i] ?? "pending",
  );
  const activeIdx = padded.findIndex((s) => s === "active");
  const label =
    activeIdx >= 0
      ? `Phase ${activeIdx + 1} (${PHASE_NAMES[activeIdx]}) active`
      : padded.every((s) => s === "done")
        ? "All 9 phases complete"
        : `${padded.filter((s) => s === "done").length}/9 phases complete`;
  return (
    <span
      role="img"
      aria-label={label}
      className={`inline-flex items-center gap-[3px] ${className}`.trim()}
    >
      {padded.map((state, idx) => (
        <span
          key={idx}
          aria-hidden
          className="inline-block rounded-[2px]"
          style={{
            width: 9,
            height: 14,
            ...barStyle(state, idx),
          }}
        />
      ))}
    </span>
  );
}
