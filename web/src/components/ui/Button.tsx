import {
  Button as SharedButton,
  type ButtonProps as SharedButtonProps,
} from "@awaken/ui";
import { cx } from "./cx";

export type ButtonVariant = "default" | "primary" | "ghost" | "danger";
export type ButtonProps = Omit<SharedButtonProps, "variant"> & {
  readonly variant?: ButtonVariant;
};

/** Awaken legacy class adapter over the shared button behavior. */
export function Button({ variant = "default", className, ...props }: ButtonProps) {
  return (
    <SharedButton
      {...props}
      variant={variant}
      className={cx("btn", variant !== "default" && variant, className)}
    />
  );
}
