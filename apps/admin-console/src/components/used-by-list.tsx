interface UsedByItem {
  namespace: string;
  id: string;
}

interface UsedByListProps {
  items: UsedByItem[];
}

export function UsedByList({ items }: UsedByListProps) {
  return (
    <div>
      <p className="mb-2">
        The following records reference this resource and would be affected:
      </p>
      <ul className="space-y-1">
        {items.map((item) => (
          <li key={`${item.namespace}/${item.id}`} className="font-mono text-xs">
            {item.namespace}/{item.id}
          </li>
        ))}
      </ul>
      <p className="mt-3">Force delete will remove this resource anyway.</p>
    </div>
  );
}
