// Switch: an on/off toggle rendered from a native checkbox (role="switch"), styled by
// `.switch` in base.css. Use for enabling a behavior or a boolean flag where a labeled
// toggle reads better than a bare checkbox. All native input attrs pass through.

import type { InputHTMLAttributes } from "react";
import { cx } from "./cx";

export type SwitchProps = Omit<InputHTMLAttributes<HTMLInputElement>, "type">;

export function Switch({ className, ...props }: SwitchProps) {
  return <input type="checkbox" role="switch" className={cx("switch", className)} {...props} />;
}
