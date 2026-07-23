import {
  Card as SharedCard,
  CardBody as SharedCardBody,
  CardHeader as SharedCardHeader,
} from "@awaken/ui";
import type { HTMLAttributes, ReactNode } from "react";
import { cx } from "./cx";

export function Card({ className, ...props }: HTMLAttributes<HTMLElement>) {
  return <SharedCard {...props} className={cx("card", className)} />;
}

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
      style={{ justifyContent: "space-between", padding: "12px 16px", ...props.style }}
    >
      {typeof title === "string" ? <h2 style={{ margin: 0, fontSize: 14 }}>{title}</h2> : title}
      {actions}
    </SharedCardHeader>
  );
}

export function CardBody({ className, ...props }: HTMLAttributes<HTMLDivElement>) {
  return <SharedCardBody {...props} className={className} style={{ padding: "0 16px 14px", ...props.style }} />;
}
