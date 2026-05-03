import type { ReactNode } from "react";
import { Eyebrow } from "./eyebrow";

export function PageHeader({
  eyebrow,
  title,
  count,
  description,
  actions,
}: {
  eyebrow?: ReactNode;
  title: ReactNode;
  count?: number | string;
  description?: ReactNode;
  actions?: ReactNode;
}) {
  return (
    <div className="mb-4 flex items-end justify-between gap-4">
      <div className="min-w-0">
        {eyebrow && <Eyebrow>{eyebrow}</Eyebrow>}
        <div className={eyebrow ? "mt-1 flex items-baseline gap-3" : "flex items-baseline gap-3"}>
          <h2 className="text-2xl font-semibold tracking-title-em text-fg-strong">
            {title}
          </h2>
          {count !== undefined && (
            <span aria-hidden className="font-mono text-sm text-fg-faint">
              {count}
            </span>
          )}
        </div>
        {description && (
          <p className="mt-1 max-w-2xl text-sm text-fg-soft">{description}</p>
        )}
      </div>
      {actions && <div className="flex shrink-0 items-center gap-2">{actions}</div>}
    </div>
  );
}
