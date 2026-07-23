// Switch: an on/off toggle rendered from a native checkbox (role="switch"), styled by
// `.switch` in base.css. Use for enabling a behavior or a boolean flag where a labeled
// toggle reads better than a bare checkbox. All native input attrs pass through.

import { Switch as SharedSwitch, type SwitchProps as SharedSwitchProps } from "@awaken/ui";
import { cx } from "./cx";

export type SwitchProps = Omit<SharedSwitchProps, "label" | "onCheckedChange">;

export function Switch({ className, ...props }: SwitchProps) {
  return <SharedSwitch className={cx("switch", className)} {...props} />;
}
