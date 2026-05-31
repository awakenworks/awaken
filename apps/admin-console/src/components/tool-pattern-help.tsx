import {
  type ToolPatternKind,
  toolPatternDescription,
  toolPatternExamples,
  toolPatternLabel,
} from "@/lib/tool-pattern-guidance";

export function ToolPatternHelp({
  kind,
  className = "mt-2 text-xs leading-5 text-fg-soft",
}: {
  kind: ToolPatternKind;
  className?: string;
}) {
  return (
    <p className={className}>
      {toolPatternDescription(kind)} Examples:{" "}
      {toolPatternExamples(kind).map((example, index) => (
        <span key={example}>
          {index > 0 ? ", " : null}
          <span className="font-mono">{example}</span>
        </span>
      ))}
      .
    </p>
  );
}

export function ToolPatternReference({
  kind,
  className = "rounded-sm border border-line bg-soft p-4",
}: {
  kind: ToolPatternKind;
  className?: string;
}) {
  return (
    <div className={className}>
      <div className="text-xs font-semibold uppercase tracking-[0.18em] text-fg-soft">
        {toolPatternLabel(kind)}
      </div>
      <ToolPatternHelp kind={kind} />
    </div>
  );
}
