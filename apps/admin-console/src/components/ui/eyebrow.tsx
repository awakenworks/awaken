import type { ReactNode } from "react";

export function Eyebrow({
  children,
  className = "",
}: {
  children: ReactNode;
  className?: string;
}) {
  return (
    <p
      className={`text-[11px] font-medium uppercase tracking-eyebrow text-fg-faint ${className}`.trim()}
    >
      {children}
    </p>
  );
}
