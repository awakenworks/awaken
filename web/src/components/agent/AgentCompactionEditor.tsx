import type { AgentConfig, CompactionStrategy } from "../../lib/api/types";
import { Card, Switch, TextField } from "../ui";
import { useApp } from "../../lib/app-state";

export default function AgentCompactionEditor({
  value,
  onChange,
}: {
  value: AgentConfig["compaction"];
  onChange: (value: CompactionStrategy | null) => void;
}) {
  const app = useApp();
  const enabled = value != null;
  const strategy = value ?? {};
  const patch = (next: Partial<CompactionStrategy>) => onChange({ ...strategy, ...next });
  return (
    <Card>
      <div className="row" style={{ justifyContent: "space-between", alignItems: "flex-start" }}>
        <div>
          <h2 className="section-title">{app.t("Context compaction strategy", "上下文压缩策略")}</h2>
          <p className="hint">{app.t(
            "Keep long Sessions within the selected model's usable context budget. Leave values blank to use model-aware safe defaults.",
            "让长 Session 保持在所选模型的可用上下文预算内；留空时使用感知模型能力的安全默认值。",
          )}</p>
        </div>
        <label className="field">
          <span>{app.t("Configure", "配置")}</span>
          <Switch checked={enabled} onChange={(event) => onChange(event.target.checked ? {} : null)} />
        </label>
      </div>
      {enabled && (
        <div className="grid-2">
          <TextField
            label={app.t("Trigger window (tokens)", "触发窗口（token）")}
            hint={app.t("Clamped to the model's usable input budget.", "不会超过模型的可用输入预算。")}
            type="number" min={1} mono
            placeholder={app.t("Derived from model", "按模型推导")}
            value={strategy.window ?? ""}
            onChange={(event) => {
              const raw = event.target.value;
              patch({ window: raw === "" ? undefined : Math.max(1, Number(raw) || 1) });
            }}
          />
          <TextField
            label={app.t("Recent messages kept verbatim", "原文保留的最近消息")}
            hint={app.t("Older context is summarized; these recent messages remain exact.", "更早的上下文会被摘要，最近这些消息保持原文。")}
            type="number" min={0} mono
            placeholder={app.t("Runtime default", "运行时默认")}
            value={strategy.keep_recent ?? ""}
            onChange={(event) => {
              const raw = event.target.value;
              patch({ keep_recent: raw === "" ? undefined : Math.max(0, Number(raw) || 0) });
            }}
          />
        </div>
      )}
    </Card>
  );
}
