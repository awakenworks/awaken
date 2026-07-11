// Field primitives: labeled text / textarea / select that auto-wire the `.field`
// + `.input` chrome and a header row with an optional "Manage ↗"-style action.
// Native-attr passthrough so call sites keep full control of the input.

import type { InputHTMLAttributes, ReactNode, SelectHTMLAttributes, TextareaHTMLAttributes } from "react";
import { cx } from "./cx";

function Label({ label, action }: { label?: ReactNode; action?: ReactNode }) {
  if (!label) return null;
  if (!action) return <label>{label}</label>;
  return (
    <label className="row" style={{ justifyContent: "space-between" }}>
      <span>{label}</span>
      {action}
    </label>
  );
}

export interface TextFieldProps extends InputHTMLAttributes<HTMLInputElement> {
  label?: ReactNode;
  action?: ReactNode;
  mono?: boolean;
}
export function TextField({ label, action, mono, className, ...props }: TextFieldProps) {
  return (
    <div className="field">
      <Label label={label} action={action} />
      <input className={cx("input", mono && "mono", className)} {...props} />
    </div>
  );
}

export interface TextAreaFieldProps extends TextareaHTMLAttributes<HTMLTextAreaElement> {
  label?: ReactNode;
  mono?: boolean;
}
export function TextAreaField({ label, mono, className, ...props }: TextAreaFieldProps) {
  return (
    <div className="field">
      <Label label={label} />
      <textarea className={cx("input", mono && "mono", className)} {...props} />
    </div>
  );
}

export interface SelectFieldProps extends SelectHTMLAttributes<HTMLSelectElement> {
  label?: ReactNode;
  action?: ReactNode;
  children: ReactNode;
}
export function SelectField({ label, action, className, children, ...props }: SelectFieldProps) {
  return (
    <div className="field">
      <Label label={label} action={action} />
      <select className={cx("input mono", className)} {...props}>
        {children}
      </select>
    </div>
  );
}
