import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import type { DreamPolicy, DreamPolicyConfig, ManagedModelPage, PlatformCapabilities } from "../../lib/api/types";
import { api, ws } from "../../lib/api/client";
import { readyDreamModels } from "../../lib/dreams";
import { useApp } from "../../lib/app-state";
import { Button, Card, SelectField, Switch, TextAreaField, TextField, useToast } from "../ui";

export default function DreamPolicyPanel({ storeId }: { storeId: string }) {
  const app = useApp();
  const toast = useToast();
  const qc = useQueryClient();
  const base = ws(`/v1/awaken/memory-stores/${encodeURIComponent(storeId)}/dream-policy`);
  const policy = useQuery({ queryKey: ["dream-policy", storeId], queryFn: () => api.get<DreamPolicy>(base) });
  const capabilities = useQuery({ queryKey: ["platform-capabilities"], queryFn: () => api.get<PlatformCapabilities>(ws("/v1/capabilities")) });
  const executableModels = useQuery({ queryKey: ["managed-models"], queryFn: () => api.get<ManagedModelPage>(ws("/v1/models")) });
  const models = readyDreamModels(capabilities.data?.dreams?.supported_models ?? [], executableModels.data?.data);
  const [draft, setDraft] = useState<DreamPolicyConfig | null>(null);
  useEffect(() => {
    if (policy.data) {
      const { enabled, interval_seconds, min_new_sessions, max_sessions, model, instructions } = policy.data;
      setDraft({ enabled, interval_seconds, min_new_sessions, max_sessions, model, instructions });
    }
  }, [policy.data]);
  const save = useMutation({
    mutationFn: () => api.put<DreamPolicy>(base, draft),
    onSuccess: (result) => {
      qc.setQueryData(["dream-policy", storeId], result);
      toast.ok(app.t("Dream automation saved.", "Dream 自动化已保存。"));
    },
    onError: (error) => toast.err(error instanceof Error ? error.message : String(error)),
  });
  if (policy.isLoading || !draft) return <Card><span className="mut">{app.t("Loading automation…", "正在加载自动化…")}</span></Card>;
  if (policy.error instanceof Error) return <Card><div className="err">{policy.error.message}</div></Card>;
  return <div className="stack">
    <Card>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <div><strong>{app.t("Automatic Dream", "自动 Dream")}</strong><div className="mut">{app.t("Periodically synthesize new completed Sessions into a new reviewable store.", "定期将新完成的会话整理为一个可检查的新记忆库。")}</div></div>
        <Switch checked={draft.enabled} onChange={(event) => setDraft({ ...draft, enabled: event.target.checked })} aria-label={app.t("Enable automatic Dream", "启用自动 Dream")} />
      </div>
    </Card>
    <div className="form-grid-2">
      <TextField label={app.t("Run every (hours)", "运行间隔（小时）")} type="number" min={1} value={Math.max(1, Math.round(draft.interval_seconds / 3600))} onChange={(event) => setDraft({ ...draft, interval_seconds: Math.max(3600, Number(event.target.value) * 3600) })} />
      <SelectField label={app.t("Model", "模型")} value={draft.model.id} onChange={(event) => setDraft({ ...draft, model: { id: event.target.value, speed: "standard" } })}>
        {models.map((model) => <option key={model} value={model}>{model}</option>)}
      </SelectField>
      <TextField label={app.t("Minimum new Sessions", "最少新增会话数")} type="number" min={1} max={draft.max_sessions} value={draft.min_new_sessions} onChange={(event) => setDraft({ ...draft, min_new_sessions: Number(event.target.value) })} />
      <TextField label={app.t("Maximum Sessions per Dream", "每次 Dream 最大会话数")} type="number" min={draft.min_new_sessions} max={capabilities.data?.dreams?.max_sessions ?? 100} value={draft.max_sessions} onChange={(event) => setDraft({ ...draft, max_sessions: Number(event.target.value) })} />
    </div>
    <TextAreaField label={app.t("Recurring guidance", "周期整理指导")} rows={7} value={draft.instructions ?? ""} onChange={(event) => setDraft({ ...draft, instructions: event.target.value || null })} />
    <Card>
      <div className="dream-review-grid">
        <span className="mut">{app.t("Next check", "下次检查")}</span><span>{policy.data?.next_due_at ? new Date(policy.data.next_due_at).toLocaleString() : app.t("After enabling", "启用后")}</span>
        <span className="mut">{app.t("Last successful cutoff", "上次成功截止")}</span><span>{policy.data?.last_completed_cutoff_at ? new Date(policy.data.last_completed_cutoff_at).toLocaleString() : app.t("Never", "从未")}</span>
      </div>
      <p className="mut">{app.t("Only a successful Dream advances the cutoff. Failed or canceled evidence is retried on a later cycle.", "只有成功完成才会推进截止点；失败或取消的证据会在之后的周期重试。")}</p>
    </Card>
    <div className="row" style={{ justifyContent: "flex-end" }}><Button variant="primary" disabled={save.isPending || !draft.model.id || draft.min_new_sessions < 1 || draft.min_new_sessions > draft.max_sessions} onClick={() => save.mutate()}>{app.t("Save automation", "保存自动化")}</Button></div>
  </div>;
}
