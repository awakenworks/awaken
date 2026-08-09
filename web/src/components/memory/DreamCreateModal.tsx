import { useMutation, useQuery } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import { useNavigate, useParams } from "react-router";
import type {
  Dream,
  ListSessionsResponse,
  ManagedModelPage,
  MemoryStore,
  Page,
  PlatformCapabilities,
} from "../../lib/api/types";
import { api, ws } from "../../lib/api/client";
import { eligibleDreamSessions, readyDreamModels } from "../../lib/dreams";
import { useApp } from "../../lib/app-state";
import { Button, Modal, Pill, SelectField, TextAreaField } from "../ui";

export default function DreamCreateModal({
  initialStoreId,
  onClose,
}: {
  initialStoreId?: string;
  onClose: () => void;
}) {
  const app = useApp();
  const nav = useNavigate();
  const { ws: wsId = "default" } = useParams();
  const [step, setStep] = useState<1 | 2 | 3>(1);
  const [memoryStoreId, setMemoryStoreId] = useState(initialStoreId ?? "");
  const [sessionIds, setSessionIds] = useState<string[]>([]);
  const [model, setModel] = useState("");
  const [instructions, setInstructions] = useState("");
  const capabilities = useQuery({
    queryKey: ["platform-capabilities"],
    queryFn: () => api.get<PlatformCapabilities>(ws("/v1/capabilities")),
  });
  const executableModels = useQuery({
    queryKey: ["managed-models", wsId],
    queryFn: () => api.get<ManagedModelPage>(ws("/v1/models")),
  });
  const stores = useQuery({
    queryKey: ["memory-stores"],
    queryFn: () => api.get<Page<MemoryStore>>(ws("/v1/memory_stores")),
  });
  const sessions = useQuery({
    queryKey: ["sessions", wsId],
    queryFn: () => api.get<ListSessionsResponse>(ws("/v1/sessions")),
  });
  const cap = capabilities.data?.dreams;
  const candidates = eligibleDreamSessions(sessions.data?.data ?? []);
  const models = readyDreamModels(cap?.supported_models ?? [], executableModels.data?.data);
  const selectedModel = model || (models.includes("claude-sonnet-5") ? "claude-sonnet-5" : models[0]) || "";
  const selectedStore = stores.data?.data.find((store) => store.id === memoryStoreId);
  const selectedSessions = useMemo(
    () => candidates.filter((session) => sessionIds.includes(session.id)),
    [candidates, sessionIds],
  );
  const create = useMutation({
    mutationFn: () => api.post<Dream>(ws("/v1/dreams"), {
      inputs: [
        { type: "memory_store", memory_store_id: memoryStoreId },
        { type: "sessions", session_ids: sessionIds },
      ],
      model: { id: selectedModel, speed: "standard" },
      ...(instructions.trim() ? { instructions: instructions.trim() } : {}),
    }),
    onSuccess: (dream) => nav(`/w/${wsId}/memory/dreams/${dream.id}`),
  });
  const canContinue = step === 1
    ? !!memoryStoreId && sessionIds.length > 0
    : step === 2
      ? !!selectedModel && instructions.length <= (cap?.max_instructions_chars ?? 4096)
      : true;

  return (
    <Modal
      title={<span className="row">{app.t("Start Dream", "启动 Dream")} <Pill tone="info">research preview</Pill></span>}
      onClose={onClose}
      width="min(900px, 96vw)"
      footer={<>
        <Button onClick={step === 1 ? onClose : () => setStep((step - 1) as 1 | 2)}>
          {step === 1 ? app.t("Cancel", "取消") : app.t("Back", "上一步")}
        </Button>
        {step < 3 ? (
          <Button variant="primary" disabled={!canContinue} onClick={() => setStep((step + 1) as 2 | 3)}>
            {app.t("Continue", "继续")}
          </Button>
        ) : (
          <Button variant="primary" disabled={create.isPending || !canContinue} onClick={() => create.mutate()}>
            {create.isPending ? app.t("Starting…", "正在启动…") : app.t("Start Dream", "启动 Dream")}
          </Button>
        )}
      </>}
    >
      <div className="stack dream-create">
        <div className="row dream-stepper">
          {[1, 2, 3].map((value) => <Pill key={value} tone={step === value ? "agent" : "neutral"}>{value} · {value === 1 ? app.t("Evidence", "证据") : value === 2 ? app.t("Guidance", "指导") : app.t("Review", "确认")}</Pill>)}
        </div>
        {step === 1 && <>
          <SelectField label={app.t("Source memory store", "来源记忆库")} value={memoryStoreId} onChange={(event) => setMemoryStoreId(event.target.value)} disabled={!!initialStoreId}>
            <option value="">{app.t("Select a memory store…", "选择记忆库…")}</option>
            {(stores.data?.data ?? []).filter((store) => !store.archived_at).map((store) => <option value={store.id} key={store.id}>{store.name} · {store.id}</option>)}
          </SelectField>
          <div className="row" style={{ justifyContent: "space-between" }}>
            <strong>{app.t("Sessions", "会话")}</strong>
            <span className="mut">{sessionIds.length}/{cap?.max_sessions ?? 100}</span>
          </div>
          <div className="dream-session-picker">
            {candidates.map((session) => {
              const checked = sessionIds.includes(session.id);
              return <label className="dream-session-option" key={session.id}>
                <input type="checkbox" checked={checked} onChange={() => setSessionIds(checked ? sessionIds.filter((id) => id !== session.id) : [...sessionIds, session.id].slice(0, cap?.max_sessions ?? 100))} />
                <span><strong>{session.title || session.id}</strong><span className="mono mut">{session.id}</span></span>
                <span className="mut">{new Date(session.updated_at).toLocaleString()}</span>
              </label>;
            })}
            {!sessions.isLoading && candidates.length === 0 && <div className="mut">{app.t("No eligible idle sessions. Running, archived, and Dream-generated sessions are excluded.", "没有可用的空闲会话。运行中、已归档及 Dream 生成的会话会被排除。")}</div>}
          </div>
        </>}
        {step === 2 && <>
          <SelectField label={app.t("Model", "模型")} value={selectedModel} onChange={(event) => setModel(event.target.value)}>
            {models.map((id) => <option value={id} key={id}>{id}</option>)}
          </SelectField>
          {models.length === 0 && <div className="err">{app.t("No Dream-compatible model is active. Connect a supported Anthropic model before starting.", "当前没有可用的 Dream 兼容模型。请先连接受支持的 Anthropic 模型。")}</div>}
          <TextAreaField
            label={app.t("Synthesis guidance (optional)", "整理指导（可选）")}
            rows={9}
            value={instructions}
            onChange={(event) => setInstructions(event.target.value)}
            hint={`${instructions.length}/${cap?.max_instructions_chars ?? 4096} · ${app.t("High-level curation guidance; it cannot widen Dream permissions.", "仅用于高层整理指导，不能扩大 Dream 权限。")}`}
          />
        </>}
        {step === 3 && <div className="stack">
          <div className="dream-review-grid">
            <span className="mut">{app.t("Source", "来源")}</span><strong>{selectedStore?.name ?? memoryStoreId}</strong>
            <span className="mut">{app.t("Sessions", "会话")}</span><strong>{selectedSessions.length}</strong>
            <span className="mut">{app.t("Model", "模型")}</span><code>{selectedModel}</code>
            <span className="mut">{app.t("Output", "输出")}</span><strong>{app.t("A new independent Memory Store", "新的独立记忆库")}</strong>
          </div>
          <div className="notice warn">{app.t("Dream is asynchronous and incurs model usage. It never changes the source store; review the output before attaching it to an Agent.", "Dream 是异步任务并会产生模型用量。它不会修改来源记忆库；绑定到 Agent 前请先检查输出。")}</div>
        </div>}
        {create.error instanceof Error && <div className="err" role="alert">{create.error.message}</div>}
      </div>
    </Modal>
  );
}
