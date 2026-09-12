// The one Outcome authoring form. Reused by the live Session control and by the
// Quickstart first run so both paths present the same three decisions and hand
// `session-outcome.ts` the same draft shape.

import { TextAreaField, TextField } from "../ui";
import { useApp } from "../../lib/app-state";
import {
  OUTCOME_ITERATIONS_MAX,
  OUTCOME_ITERATIONS_MIN,
  type OutcomeDraft,
  clampOutcomeIterations,
} from "../../lib/session-outcome";

export default function OutcomeFields({
  draft,
  onChange,
  disabled,
}: {
  draft: OutcomeDraft;
  onChange: (draft: OutcomeDraft) => void;
  disabled?: boolean;
}) {
  const app = useApp();
  return (
    <>
      <TextAreaField
        label={app.t("Deliverable", "交付物")}
        hint={app.t(
          "What the Agent should produce. The grader reads this as the task specification.",
          "Agent 应当产出什么。评价者会把它当作任务说明来读。",
        )}
        rows={2}
        disabled={disabled}
        placeholder={app.t(
          "A sourced brief that answers the question and names its open gaps.",
          "一份有来源支撑、回答了问题并指出未尽事项的简报。",
        )}
        value={draft.description}
        onChange={(event) => onChange({ ...draft, description: event.target.value })}
      />
      <TextAreaField
        label={app.t("Grading rubric", "评分标准")}
        hint={app.t(
          "Plain text or Markdown. One checkable criterion per line reads best; the grader quotes these back as feedback.",
          "纯文本或 Markdown。每行一条可核查的标准最易读；评价者会引用它们作为反馈。",
        )}
        rows={5}
        disabled={disabled}
        placeholder={app.t(
          "- Every non-obvious claim carries an inline source.\n- Conflicting sources are surfaced, not smoothed over.\n- Closes with what remains uncertain.",
          "- 每条非显而易见的结论都标注了来源。\n- 来源冲突被指出而不是被抹平。\n- 结尾说明仍然存在的不确定性。",
        )}
        value={draft.rubric}
        onChange={(event) => onChange({ ...draft, rubric: event.target.value })}
      />
      <TextField
        label={app.t("Revision limit", "修订轮次上限")}
        hint={app.t(
          "Eval→revision cycles before the Agent gives up. Reaching the limit ends the Outcome unsatisfied rather than looping.",
          "Agent 放弃前的「评价→修订」轮次。达到上限即以未达标结束，而不是继续循环。",
        )}
        type="number"
        min={OUTCOME_ITERATIONS_MIN}
        max={OUTCOME_ITERATIONS_MAX}
        step={1}
        disabled={disabled}
        value={draft.maxIterations}
        onChange={(event) => onChange({
          ...draft,
          maxIterations: clampOutcomeIterations(Number(event.target.value)),
        })}
      />
    </>
  );
}
