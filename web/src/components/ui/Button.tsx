// Button primitive: native-attr passthrough + a typed `variant` that maps to the
// legacy `.btn` BEM classes (styling lives in base.css). Mirrors oversight-next's
// Button so call sites read the same everywhere.

import type { ButtonHTMLAttributes, ReactNode } from "react";
import { cx } from "./cx";

export type ButtonVariant = "default" | "primary" | "ghost" | "danger";

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: ButtonVariant;
  icon?: ReactNode;
}

export function Button({ variant = "default", icon, className, children, ...props }: ButtonProps) {
  return (
    <button className={cx("btn", variant !== "default" && variant, className)} {...props}>
      {icon}
      {children}
    </button>
  );
}
