import { CardHeader as SharedCardHeader } from "@awaken/ui";
import type { HTMLAttributes, ReactNode } from "react";
import { cx } from "./cx";

export function CardHeader({
  title,
  actions,
  className,
  ...props
}: HTMLAttributes<HTMLDivElement> & { readonly title?: ReactNode; readonly actions?: ReactNode }) {
  return (
    <SharedCardHeader
      {...props}
      className={cx("row", className)}
    >
      {typeof title === "string" ? <h2>{title}</h2> : title}
      {actions}
    </SharedCardHeader>
  );
}
