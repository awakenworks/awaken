import {
  SelectField as SharedSelectField,
  TextAreaField as SharedTextAreaField,
  TextField as SharedTextField,
} from "@awaken/ui";
import type {
  InputHTMLAttributes,
  ReactNode,
  SelectHTMLAttributes,
  TextareaHTMLAttributes,
} from "react";
import { cx } from "./cx";

export interface TextFieldProps extends InputHTMLAttributes<HTMLInputElement> {
  readonly label?: ReactNode;
  readonly action?: ReactNode;
  readonly hint?: ReactNode;
  readonly mono?: boolean;
}

export function TextField({
  action,
  hint,
  mono,
  className,
  ...props
}: TextFieldProps) {
  return (
    <SharedTextField
      {...props}
      action={action}
      className={cx("input", mono && "mono", className)}
      fieldClassName="field"
      help={hint}
      helpClassName="mut"
      labelClassName={action ? "row" : undefined}
    />
  );
}

export interface TextAreaFieldProps extends TextareaHTMLAttributes<HTMLTextAreaElement> {
  readonly label?: ReactNode;
  readonly hint?: ReactNode;
  readonly mono?: boolean;
}

export function TextAreaField({ hint, mono, className, ...props }: TextAreaFieldProps) {
  return (
    <SharedTextAreaField
      {...props}
      className={cx("input", mono && "mono", className)}
      fieldClassName="field"
      help={hint}
      helpClassName="mut"
    />
  );
}

export interface SelectFieldProps extends SelectHTMLAttributes<HTMLSelectElement> {
  readonly label?: ReactNode;
  readonly action?: ReactNode;
  readonly mono?: boolean;
  readonly children: ReactNode;
}

export function SelectField({
  action,
  mono,
  className,
  children,
  ...props
}: SelectFieldProps) {
  return (
    <SharedSelectField
      {...props}
      action={action}
      className={cx("input", mono && "mono", className)}
      fieldClassName="field"
      labelClassName={action ? "row" : undefined}
    >
      {children}
    </SharedSelectField>
  );
}
