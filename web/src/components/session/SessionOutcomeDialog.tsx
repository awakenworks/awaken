// @formal surface requirement=runtime.outcomes.outcome_lifecycle
// @formal reason: The live-Session half of Outcome authoring: it owns the draft while open and hands
// @formal reason: sending to the one Session send seam that already owns idempotency. Classification only,
// @formal reason: not formal evidence.
// The live-Session half of Outcome authoring. It owns only the draft while the
// dialog is open; the Session surface owns sending, because an Outcome is an
// ordinary inbound event on the one send seam that already handles idempotency,
// receipts, and projection reconciliation.

import { Info } from "@awaken/ui/icons";
import { InlineNotice, Button } from "@awaken/ui";
import { useState } from "react";
import { Modal } from "../ui";
import { useApp } from "../../lib/app-state";
import {
  type OutcomeDraft,
  emptyOutcomeDraft,
  outcomeDraftComplete,
} from "../../lib/session-outcome";
import OutcomeFields from "./OutcomeFields";

export default function SessionOutcomeDialog({
  pending,
  onClose,
  onSubmit,
}: {
  pending: boolean;
  onClose: () => void;
  onSubmit: (draft: OutcomeDraft) => void;
}) {
  const app = useApp();
  const [draft, setDraft] = useState<OutcomeDraft>(emptyOutcomeDraft);
  return (
    <Modal
      title={app.t("Define an outcome", "定义结果")}
      width="min(640px, 92vw)"
      onClose={() => !pending && onClose()}
      footer={
        <>
          <Button disabled={pending} onClick={onClose}>{app.t("Cancel", "取消")}</Button>
          <Button
            variant="primary"
            disabled={pending || !outcomeDraftComplete(draft)}
            onClick={() => onSubmit(draft)}
          >
            {pending ? app.t("Sending…", "正在发送…") : app.t("Define outcome", "定义结果")}
          </Button>
        </>
      }
    >
      <InlineNotice tone="info" icon={<Info />}>
        <span>{app.t(
          "The Agent works toward this deliverable while a separately contextualized grader scores it against the rubric. Feedback drives revisions until the rubric is satisfied or the revision limit is reached.",
          "Agent 会朝这个交付物工作，由独立上下文的评价者对照评分标准打分；反馈驱动修订，直到达标或用完轮次。",
        )}</span>
      </InlineNotice>
      <OutcomeFields draft={draft} onChange={setDraft} disabled={pending} />
    </Modal>
  );
}
