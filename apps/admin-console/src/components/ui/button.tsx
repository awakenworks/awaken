import { forwardRef, type ButtonHTMLAttributes, type ReactNode } from "react";

export type ButtonVariant = "primary" | "secondary" | "ghost" | "danger" | "link";
export type ButtonSize = "sm" | "md";

const VARIANT: Record<ButtonVariant, string> = {
  primary:
    "bg-fg-strong text-bg hover:bg-fg disabled:bg-fg-soft",
  secondary:
    "border border-line-strong bg-surface text-fg hover:bg-soft disabled:opacity-60",
  ghost:
    "bg-transparent text-fg-soft hover:text-fg hover:bg-soft",
  danger:
    "bg-tone-error text-bg hover:opacity-90 disabled:opacity-60",
  link:
    "bg-transparent text-link hover:text-link-hover underline-offset-2 hover:underline",
};

const SIZE: Record<ButtonSize, string> = {
  sm: "h-7 px-2.5 text-xs",
  md: "h-9 px-3 text-sm",
};

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: ButtonVariant;
  size?: ButtonSize;
  loading?: boolean;
  /** Optional left icon (rendered before children). */
  iconLeft?: ReactNode;
}

/**
 * Single source of truth for buttons. Replaces the ad-hoc `<button
 * className="rounded-xl bg-fg-strong px-4 py-2 ...">` repetition that drifted
 * across pages with different paddings, radii, and hover treatments.
 */
export const Button = forwardRef<HTMLButtonElement, ButtonProps>(function Button(
  {
    variant = "primary",
    size = "md",
    loading = false,
    iconLeft,
    type = "button",
    disabled,
    className = "",
    children,
    ...rest
  },
  ref,
) {
  const cls = [
    "inline-flex items-center justify-center gap-1.5 rounded-md font-medium transition-colors",
    "focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus",
    "disabled:cursor-not-allowed",
    SIZE[size],
    VARIANT[variant],
    className,
  ]
    .join(" ")
    .trim();
  return (
    <button
      ref={ref}
      type={type}
      disabled={disabled || loading}
      className={cls}
      {...rest}
    >
      {iconLeft && <span aria-hidden>{iconLeft}</span>}
      <span>{loading ? "…" : children}</span>
    </button>
  );
});
