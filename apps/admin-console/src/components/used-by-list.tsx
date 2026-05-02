interface UsedByItem {
  namespace: string;
  id: string;
}

interface UsedByListProps {
  items: UsedByItem[];
}

/** Group dependents by namespace for a tighter card. e.g.
 *  `agents` (3): research-assistant, reviewer, deployer */
function groupByNamespace(items: UsedByItem[]): Map<string, string[]> {
  const out = new Map<string, string[]>();
  for (const item of items) {
    const list = out.get(item.namespace) ?? [];
    list.push(item.id);
    out.set(item.namespace, list);
  }
  return out;
}

export function UsedByList({ items }: UsedByListProps) {
  const grouped = groupByNamespace(items);
  return (
    <div className="space-y-3">
      <div className="rounded-md border border-tone-warn/30 bg-tone-warn/10 px-3 py-2.5">
        <div className="flex items-start gap-2 text-sm">
          <span
            aria-hidden
            className="mt-0.5 inline-flex h-5 w-5 shrink-0 items-center justify-center rounded-pill bg-tone-warn/15 text-xs font-bold text-tone-warn"
          >
            !
          </span>
          <div>
            <strong className="text-fg-strong">
              {items.length} record{items.length === 1 ? "" : "s"} still reference this resource.
            </strong>
            <span className="ml-1 text-fg-soft">
              Deleting will leave them with a dangling pointer.
            </span>
          </div>
        </div>
      </div>

      <div className="space-y-2">
        {Array.from(grouped.entries()).map(([namespace, ids]) => (
          <div key={namespace} className="rounded-md border border-line bg-soft px-3 py-2">
            <div className="flex items-center gap-2 text-xs">
              <span className="font-mono font-medium text-fg-strong">{namespace}</span>
              <span className="text-fg-faint">·</span>
              <span className="text-fg-soft">{ids.length}</span>
            </div>
            <ul className="mt-1.5 flex flex-wrap gap-1.5">
              {ids.slice(0, 8).map((id) => (
                <li
                  key={id}
                  className="rounded-pill bg-bg px-2 py-0.5 font-mono text-[11px] text-fg"
                >
                  {id}
                </li>
              ))}
              {ids.length > 8 && (
                <li className="text-[11px] text-fg-soft">+ {ids.length - 8} more</li>
              )}
            </ul>
          </div>
        ))}
      </div>

      <p className="text-xs text-fg-soft">
        <strong className="text-fg">Force delete</strong> will remove this
        resource regardless. Affected records will need their pointer fixed.
      </p>
    </div>
  );
}
