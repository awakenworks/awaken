import { CopyButton as SharedCopyButton } from "@awaken/ui";
import { cx } from "./cx";

export type CopyButtonProps = {
  readonly value: string;
  readonly label?: string;
  readonly copiedLabel?: string;
  readonly className?: string;
};

/** Awaken text-button presentation over the shared clipboard state machine. */
export function CopyButton({
  value,
  label = "copy",
  copiedLabel = "copied",
  className,
}: CopyButtonProps) {
  return (
    <SharedCopyButton
      className={cx("btn", "ghost", "copy-btn", className)}
      copiedIcon={<span aria-hidden="true">{copiedLabel}</span>}
      copiedLabel={copiedLabel}
      icon={<span aria-hidden="true">{label}</span>}
      label={label}
      value={value}
    />
  );
}
