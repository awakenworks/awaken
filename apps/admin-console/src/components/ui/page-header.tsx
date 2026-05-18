import type { ReactNode } from "react";
import { Eyebrow } from "./eyebrow";

/* Spec (awaken-admin.html .ph): horizontal page header.
 *   Left column  · gap-1 · eyebrow (10px caps) → h1 (22px/700) → sub (13px)
 *   Right column · margin-left auto · actions row (gap-2)
 * Linear stack, no "min-w-0 sometimes mt-1 sometimes ml-auto" — one flow.
 * Public API (eyebrow/title/count/description/actions) preserved so all
 * 3 page consumers + test continue to work without changes. */
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
    <div className="mb-6 flex items-start justify-between gap-4">
      <div className="flex min-w-0 flex-col gap-1">
        {eyebrow && <Eyebrow>{eyebrow}</Eyebrow>}
        <div className="flex items-baseline gap-3">
          <h1 className="text-[22px] font-bold tracking-title-em text-fg-strong">
            {title}
          </h1>
          {count !== undefined && (
            <span aria-hidden className="font-mono text-sm text-fg-faint">
              {count}
            </span>
          )}
        </div>
        {description && (
          <p className="max-w-2xl text-[13px] leading-relaxed text-fg-soft">
            {description}
          </p>
        )}
      </div>
      {actions && (
        <div className="flex shrink-0 items-center gap-2">{actions}</div>
      )}
    </div>
  );
}
