// Card compound primitive (oversight-next pattern): Card + CardHeader + CardBody.
// Thin wrappers over the `.card` class so surfaces compose instead of repeating
// markup.

import type { HTMLAttributes, ReactNode } from "react";
import { cx } from "./cx";

export function Card({ className, children, ...props }: HTMLAttributes<HTMLDivElement>) {
  return (
    <div className={cx("card", className)} {...props}>
      {children}
    </div>
  );
}

export function CardHeader({
  title,
  actions,
  className,
  ...props
}: HTMLAttributes<HTMLDivElement> & { title?: ReactNode; actions?: ReactNode }) {
  return (
    <div className={cx("row", className)} style={{ justifyContent: "space-between", padding: "12px 16px" }} {...props}>
      {typeof title === "string" ? <h2 style={{ margin: 0, fontSize: 14 }}>{title}</h2> : title}
      {actions}
    </div>
  );
}

export function CardBody({ className, children, ...props }: HTMLAttributes<HTMLDivElement>) {
  return (
    <div className={cx(className)} style={{ padding: "0 16px 14px" }} {...props}>
      {children}
    </div>
  );
}
