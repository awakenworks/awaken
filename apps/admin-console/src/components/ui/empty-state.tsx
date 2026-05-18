import type { ReactNode } from "react";

export function EmptyState({
  icon,
  title,
  description,
  actions,
  className = "",
}: {
  icon?: ReactNode;
  title: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  className?: string;
}) {
  return (
    <div
      className={`flex flex-col items-center gap-2.5 rounded-sm border border-dashed border-line-strong px-6 py-10 text-center ${className}`.trim()}
    >
      {icon && (
        <div
          aria-hidden
          className="flex h-9 w-9 items-center justify-center rounded-full border border-line-strong text-fg-faint"
        >
          {icon}
        </div>
      )}
      <div className="text-base font-medium text-fg-strong">{title}</div>
      {description && (
        <div className="max-w-md text-sm text-fg-soft">{description}</div>
      )}
      {actions && <div className="mt-2 flex flex-wrap items-center gap-2">{actions}</div>}
    </div>
  );
}
