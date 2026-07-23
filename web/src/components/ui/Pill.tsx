import {
  Badge as SharedBadge,
  StatusPill,
  type UiTone,
} from "@awaken/ui";
import type { HTMLAttributes } from "react";
import { cx, type Tone } from "./cx";

const TONE: Record<Tone, UiTone> = {
  ok: "success",
  warn: "warning",
  danger: "danger",
  agent: "agent",
  neutral: "neutral",
  info: "info",
};

export interface PillProps extends HTMLAttributes<HTMLSpanElement> {
  readonly tone?: Tone;
  readonly dot?: boolean;
}

export function Pill({ tone = "neutral", dot, className, children, ...props }: PillProps) {
  return (
    <StatusPill {...props} tone={TONE[tone]} className={cx("pill", tone, className)}>
      {dot ? <span className="dot" /> : null}
      {children}
    </StatusPill>
  );
}

export function Badge({ className, ...props }: HTMLAttributes<HTMLSpanElement>) {
  return <SharedBadge {...props} className={cx("nav-badge", className)} />;
}
