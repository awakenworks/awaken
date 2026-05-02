import { forwardRef, type InputHTMLAttributes, type TextareaHTMLAttributes } from "react";

const INPUT_BASE =
  "w-full rounded-md border border-line-strong bg-surface px-3 py-2 text-sm text-fg-strong outline-none transition-colors placeholder:text-fg-faint " +
  "focus:border-link focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus " +
  "disabled:bg-muted disabled:text-fg-soft disabled:cursor-not-allowed";

export interface InputProps extends InputHTMLAttributes<HTMLInputElement> {
  /** When true, render with monospace family — for ids, paths, JSON values. */
  mono?: boolean;
}

/** Standard text input — replaces ad-hoc `<input className="rounded-xl …">`. */
export const Input = forwardRef<HTMLInputElement, InputProps>(function Input(
  { className = "", mono = false, type = "text", ...rest },
  ref,
) {
  return (
    <input
      ref={ref}
      type={type}
      className={[INPUT_BASE, mono ? "font-mono" : "", className]
        .join(" ")
        .trim()}
      {...rest}
    />
  );
});

export interface TextareaProps
  extends TextareaHTMLAttributes<HTMLTextAreaElement> {
  mono?: boolean;
}

export const Textarea = forwardRef<HTMLTextAreaElement, TextareaProps>(function Textarea(
  { className = "", mono = false, rows = 4, ...rest },
  ref,
) {
  return (
    <textarea
      ref={ref}
      rows={rows}
      className={[INPUT_BASE, mono ? "font-mono" : "", className]
        .join(" ")
        .trim()}
      {...rest}
    />
  );
});
