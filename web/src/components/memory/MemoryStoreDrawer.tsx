import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import type {
  MemoryEntry,
  MemoryStore,
  MemoryStoreConfig,
  MemoryVersion,
  Page,
} from "../../lib/api/types";
import { api, ws } from "../../lib/api/client";
import { useApp } from "../../lib/app-state";
import {
  Button,
  Card,
  Drawer,
  EmptyState,
  Pill,
  Segmented,
  SkeletonRows,
  Switch,
  TextField,
  useConfirm,
  useToast,
} from "../ui";

type Tab = "content" | "policies" | "history";

function QueryError({ message, retry }: { message: string; retry: () => void }) {
  const app = useApp();
  return (
    <EmptyState
      title={app.t("This data could not be loaded", "数据加载失败")}
      hint={message}
      action={<Button onClick={retry}>{app.t("Try again", "重试")}</Button>}
    />
  );
}

export default function MemoryStoreDrawer({ store, onClose }: { store: MemoryStore; onClose: () => void }) {
  const app = useApp();
  const toast = useToast();
  const confirm = useConfirm();
  const qc = useQueryClient();
  const [tab, setTab] = useState<Tab>("content");
  const base = ws(`/v1/memory_stores/${encodeURIComponent(store.id)}`);
  const config = useQuery({
    queryKey: ["memory-store-config", store.id],
    queryFn: () => api.get<MemoryStoreConfig>(`${base}/config`),
  });
  const memories = useQuery({
    queryKey: ["memory-store-entries", store.id],
    queryFn: () => api.get<Page<MemoryEntry>>(`${base}/memories?view=full&limit=100`),
    enabled: tab === "content",
  });
  const versions = useQuery({
    queryKey: ["memory-store-versions", store.id],
    queryFn: () => api.get<Page<MemoryVersion>>(`${base}/memory_versions`),
    enabled: tab === "history",
  });

  const [recallEnabled, setRecallEnabled] = useState(true);
  const [maxResults, setMaxResults] = useState("10");
  const [extractionEnabled, setExtractionEnabled] = useState(true);
  const [retentionDays, setRetentionDays] = useState("");
  useEffect(() => {
    if (!config.data) return;
    setRecallEnabled(config.data.recall_policy.enabled);
    setMaxResults(String(config.data.recall_policy.max_results));
    setExtractionEnabled(config.data.extraction_policy.enabled);
    setRetentionDays(config.data.retention_policy.retention_days == null ? "" : String(config.data.retention_policy.retention_days));
  }, [config.data]);

  const policyDirty = !!config.data && (
    recallEnabled !== config.data.recall_policy.enabled
    || Number(maxResults) !== config.data.recall_policy.max_results
    || extractionEnabled !== config.data.extraction_policy.enabled
    || (retentionDays === "" ? null : Number(retentionDays)) !== (config.data.retention_policy.retention_days ?? null)
  );
  const policyValid = Number.isInteger(Number(maxResults)) && Number(maxResults) > 0
    && (retentionDays === "" || (Number.isInteger(Number(retentionDays)) && Number(retentionDays) >= 0));
  const savePolicies = useMutation({
    mutationFn: () => api.post<MemoryStoreConfig>(`${base}/config`, {
      expected_config_version: config.data?.version,
      recall_policy: { enabled: recallEnabled, max_results: Number(maxResults) },
      extraction_policy: { enabled: extractionEnabled },
      retention_policy: { ...(retentionDays === "" ? {} : { retention_days: Number(retentionDays) }) },
    }),
    onSuccess: (next) => {
      qc.setQueryData(["memory-store-config", store.id], next);
      toast.ok(app.t(`Published policy version ${next.version}.`, `已发布策略版本 ${next.version}。`));
    },
    onError: (cause) => toast.err(cause instanceof Error ? cause.message : String(cause)),
  });
  const redact = useMutation({
    mutationFn: (versionId: string) => api.post(`${base}/memory_versions/${encodeURIComponent(versionId)}/redact`),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["memory-store-versions", store.id] });
      toast.ok(app.t("Version content redacted.", "版本内容已遮盖。"));
    },
    onError: (cause) => toast.err(cause instanceof Error ? cause.message : String(cause)),
  });

  return (
    <Drawer title={`${store.name || store.id} · ${app.t("Memory Store", "记忆库")}`} onClose={onClose}>
      <div className="stack">
        <div className="row" style={{ justifyContent: "space-between", alignItems: "flex-start" }}>
          <div>
            <div className="mono">{store.id}</div>
            <div className="mut">{store.description || app.t("No description", "暂无描述")}</div>
          </div>
          {store.archived_at ? <Pill tone="neutral">{app.t("Archived", "已归档")}</Pill> : <Pill tone="ok">{app.t("Active", "使用中")}</Pill>}
        </div>

        <Segmented
          value={tab}
          onChange={setTab}
          options={[
            { value: "content", label: app.t("Content", "内容") },
            { value: "policies", label: app.t("Policies", "策略") },
            { value: "history", label: app.t("History", "版本历史") },
          ]}
        />

        {tab === "content" && memories.error instanceof Error && (
          <QueryError message={memories.error.message} retry={() => void memories.refetch()} />
        )}
        {tab === "content" && !memories.error && (
          <Card style={{ padding: 0, overflow: "auto" }}>
            <table className="table">
              <thead><tr><th>{app.t("Path", "路径")}</th><th>{app.t("Content", "内容")}</th><th>{app.t("Size", "大小")}</th><th>{app.t("Updated", "更新时间")}</th></tr></thead>
              {memories.isLoading ? <SkeletonRows rows={4} cols={4} /> : (
                <tbody>
                  {(memories.data?.data ?? []).map((memory) => (
                    <tr key={memory.id}>
                      <td className="mono">{memory.path}</td>
                      <td style={{ minWidth: 260 }}>
                        <details>
                          <summary style={{ cursor: "pointer" }}>{app.t("View content", "查看内容")}</summary>
                          <pre className="mono" style={{ whiteSpace: "pre-wrap", maxHeight: 260, overflow: "auto" }}>{memory.content || app.t("Empty", "空内容")}</pre>
                        </details>
                      </td>
                      <td>{memory.content_size_bytes} B</td>
                      <td className="mut">{new Date(memory.updated_at).toLocaleString()}</td>
                    </tr>
                  ))}
                  {(memories.data?.data.length ?? 0) === 0 && (
                    <tr><td colSpan={4} className="mut">{app.t("This store has no memory entries yet.", "该记忆库还没有内容。")}</td></tr>
                  )}
                </tbody>
              )}
            </table>
          </Card>
        )}

        {tab === "policies" && config.error instanceof Error && (
          <QueryError message={config.error.message} retry={() => void config.refetch()} />
        )}
        {tab === "policies" && !config.error && config.isLoading && <Card>{app.t("Loading policies…", "正在加载策略…")}</Card>}
        {tab === "policies" && config.data && (
          <>
            <Card>
              <div className="row" style={{ justifyContent: "space-between" }}>
                <div><strong>{app.t("Recall", "召回")}</strong><div className="mut">{app.t("Search this store before each run and add relevant memories to context.", "每次运行前检索该记忆库，并把相关记忆加入上下文。")}</div></div>
                <Switch checked={recallEnabled} onChange={(event) => setRecallEnabled(event.target.checked)} aria-label={app.t("Recall enabled", "启用召回")} />
              </div>
              <TextField type="number" min={1} label={app.t("Maximum recalled memories", "最大召回条数")} value={maxResults} disabled={!recallEnabled} onChange={(event) => setMaxResults(event.target.value)} />
            </Card>
            <Card>
              <div className="row" style={{ justifyContent: "space-between" }}>
                <div><strong>{app.t("Automatic extraction", "自动提取")}</strong><div className="mut">{app.t("Extract durable memories from completed turns.", "从完成的对话轮次中提取持久记忆。")}</div></div>
                <Switch checked={extractionEnabled} onChange={(event) => setExtractionEnabled(event.target.checked)} aria-label={app.t("Extraction enabled", "启用提取")} />
              </div>
              <div style={{ marginTop: 8 }}><Pill tone={extractionEnabled ? "ok" : "neutral"}>{extractionEnabled ? app.t("Extraction active", "提取已启用") : app.t("Extraction disabled", "提取已关闭")}</Pill></div>
            </Card>
            <Card>
              <TextField type="number" min={0} label={app.t("Retention days", "保留天数")} hint={app.t("Leave blank to retain indefinitely. Used when a store is deleted.", "留空表示永久保留；删除记忆库时按此期限清理。") } value={retentionDays} onChange={(event) => setRetentionDays(event.target.value)} />
            </Card>
            <div className="row" style={{ justifyContent: "space-between" }}>
              <span className="mut">{app.t(`Current policy version: ${config.data.version}`, `当前策略版本：${config.data.version}`)}</span>
              <Button variant="primary" disabled={!policyDirty || !policyValid || savePolicies.isPending} onClick={() => savePolicies.mutate()}>{app.t("Publish policy update", "发布策略更新")}</Button>
            </div>
            {!policyValid && <div className="err">{app.t("Enter positive whole numbers for policy limits.", "策略限制必须是有效的非负整数。")}</div>}
          </>
        )}

        {tab === "history" && versions.error instanceof Error && (
          <QueryError message={versions.error.message} retry={() => void versions.refetch()} />
        )}
        {tab === "history" && !versions.error && (
          <Card style={{ padding: 0, overflow: "auto" }}>
            <table className="table">
              <thead><tr><th>{app.t("Version", "版本")}</th><th>{app.t("Path", "路径")}</th><th>{app.t("Operation", "操作")}</th><th>{app.t("Created", "创建时间")}</th><th /></tr></thead>
              {versions.isLoading ? <SkeletonRows rows={4} cols={5} /> : (
                <tbody>
                  {(versions.data?.data ?? []).map((version) => (
                    <tr key={version.id}>
                      <td className="mono">{version.id}</td>
                      <td className="mono">{version.path}</td>
                      <td><Pill tone={version.operation === "deleted" ? "neutral" : "agent"}>{version.operation}</Pill></td>
                      <td className="mut">{new Date(version.created_at).toLocaleString()}</td>
                      <td style={{ textAlign: "right" }}>{version.redacted_at ? <Pill tone="neutral">{app.t("Redacted", "已遮盖")}</Pill> : <Button variant="danger" disabled={redact.isPending} onClick={async () => {
                        const approved = await confirm({ title: app.t("Redact this version?", "遮盖该版本？"), body: app.t("Its historical content will be permanently removed. The audit record remains.", "历史内容将被永久移除，但审计记录会保留。"), confirmLabel: app.t("Redact", "遮盖"), danger: true });
                        if (approved) redact.mutate(version.id);
                      }}>{app.t("Redact", "遮盖")}</Button>}</td>
                    </tr>
                  ))}
                  {(versions.data?.data.length ?? 0) === 0 && <tr><td colSpan={5} className="mut">{app.t("No memory versions yet.", "暂无记忆版本。")}</td></tr>}
                </tbody>
              )}
            </table>
          </Card>
        )}
      </div>
    </Drawer>
  );
}
