import { CopyButton as SharedCopyButton } from "@awaken/ui";
import { cx } from "./cx";

export type CopyButtonProps = {
  readonly value: string;
  readonly label?: string;
  readonly visibleLabel?: string;
  readonly copiedLabel?: string;
  readonly visibleCopiedLabel?: string;
  readonly className?: string;
};

/** Awaken text-button presentation over the shared clipboard state machine. */
export function CopyButton({
  value,
  label = "copy",
  visibleLabel,
  copiedLabel = "copied",
  visibleCopiedLabel,
  className,
}: CopyButtonProps) {
  return (
    <SharedCopyButton
      className={cx("btn", "ghost", "copy-btn", className)}
      copiedIcon={<span aria-hidden="true">{visibleCopiedLabel ?? copiedLabel}</span>}
      copiedLabel={copiedLabel}
      icon={<span aria-hidden="true">{visibleLabel ?? label}</span>}
      label={label}
      value={value}
    />
  );
}
