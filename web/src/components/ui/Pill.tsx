// Pill / Badge primitives: a tone-keyed status label and a count badge. Both are
// thin native-attr wrappers over the `.pill` / `.nav-badge` classes.

import type { HTMLAttributes, ReactNode } from "react";
import { cx, type Tone } from "./cx";

export interface PillProps extends HTMLAttributes<HTMLSpanElement> {
  tone?: Tone;
  dot?: boolean;
}

export function Pill({ tone = "neutral", dot, className, children, ...props }: PillProps) {
  return (
    <span className={cx("pill", tone, className)} {...props}>
      {dot && <span className="dot" />}
      {children}
    </span>
  );
}

export function Badge({ children, className, ...props }: HTMLAttributes<HTMLSpanElement>) {
  return (
    <span className={cx("nav-badge", className)} {...props}>
      {children as ReactNode}
    </span>
  );
}
