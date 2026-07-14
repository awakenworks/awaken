// Field primitives: labeled text / textarea / select that auto-wire the `.field`
// + `.input` chrome and a header row with an optional "Manage ↗"-style action.
// Native-attr passthrough so call sites keep full control of the input.

import type { InputHTMLAttributes, ReactNode, SelectHTMLAttributes, TextareaHTMLAttributes } from "react";
import { cx } from "./cx";

function Label({ label, action, hint }: { label?: ReactNode; action?: ReactNode; hint?: ReactNode }) {
  if (!label) return null;
  const head = !action ? (
    <label>{label}</label>
  ) : (
    <label className="row" style={{ justifyContent: "space-between" }}>
      <span>{label}</span>
      {action}
    </label>
  );
  if (hint == null) return head;
  return (
    <>
      {head}
      {/* Sub-label clarifying a field's purpose/audience (e.g. delegation vs behavior). */}
      <span className="mut" style={{ fontSize: 11, display: "block", marginBottom: 4 }}>{hint}</span>
    </>
  );
}

export interface TextFieldProps extends InputHTMLAttributes<HTMLInputElement> {
  label?: ReactNode;
  action?: ReactNode;
  hint?: ReactNode;
  mono?: boolean;
}
export function TextField({ label, action, hint, mono, className, ...props }: TextFieldProps) {
  return (
    <div className="field">
      <Label label={label} action={action} hint={hint} />
      <input className={cx("input", mono && "mono", className)} {...props} />
    </div>
  );
}

export interface TextAreaFieldProps extends TextareaHTMLAttributes<HTMLTextAreaElement> {
  label?: ReactNode;
  hint?: ReactNode;
  mono?: boolean;
}
export function TextAreaField({ label, hint, mono, className, ...props }: TextAreaFieldProps) {
  return (
    <div className="field">
      <Label label={label} hint={hint} />
      <textarea className={cx("input", mono && "mono", className)} {...props} />
    </div>
  );
}

export interface SelectFieldProps extends SelectHTMLAttributes<HTMLSelectElement> {
  label?: ReactNode;
  action?: ReactNode;
  mono?: boolean;
  children: ReactNode;
}
export function SelectField({ label, action, mono, className, children, ...props }: SelectFieldProps) {
  return (
    <div className="field">
      <Label label={label} action={action} />
      <select className={cx("input", mono && "mono", className)} {...props}>
        {children}
      </select>
    </div>
  );
}
