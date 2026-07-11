// Project · Agent editor: authors the agent object against our own config plane
// (PUT /v1/config/agents/:id). The object model IS the managed `/v1/agents` object
// (name / model / system / tools / mcp_servers / skills / multiagent / metadata)
// plus our extension block (plugins / plugin_config / context_policy / max_steps) —
// consistent with the SDK object, extended with our differentiated value. Policy
// (permission / state_machine / deferred-tools / generative-ui) lives under
// plugin_config. `Publish` compiles + installs the config so sessions run it.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useMemo, useState } from "react";
import { useNavigate, useParams } from "react-router";
import Drawer from "../components/ui/Drawer";
import { useToast } from "../components/ui/Toast";
import { Button, Card, CheckPicker, Pill, SchemaForm, TextAreaField, TextField } from "../components/ui";
import type { JsonSchema } from "../components/ui";
import SandboxPane from "../components/session/SandboxPane";
import PermissionEditor from "../components/agent/PermissionEditor";
import { api, isAbsent } from "../lib/api/client";
import type {
  AgentConfig,
  AgentConfigItem,
  ContextPolicy,
  PermissionConfig,
  PublishResult,
} from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { useCapabilities } from "../lib/useCapabilities";
import { useModels } from "../lib/useModels";
import { useUnsavedGuard } from "../lib/useUnsavedGuard";
import ModelsSurface from "./models";

type Tab = "basics" | "context" | "tools" | "plugins" | "sandbox";

const BLANK: AgentConfig = {
  id: "",
  name: "",
  model: { id: "" },
  system: "You are a helpful coding agent.",
  metadata: {},
  tools: [],
  mcp_servers: [],
  skills: [],
  max_steps: 8,
  plugins: [],
  plugin_config: {},
  context_policy: { kind: "keep_all" },
};

function modelId(m: AgentConfig["model"]): string {
  return typeof m === "string" ? m : (m?.id ?? "");
}

/** A minimal add/remove editor for a string list (tools, plugins). */
function ListEditor({
  values,
  onChange,
  placeholder,
}: {
  values: string[];
  onChange: (next: string[]) => void;
  placeholder: string;
}) {
  const [draft, setDraft] = useState("");
  const add = () => {
    const v = draft.trim();
    if (v && !values.includes(v)) onChange([...values, v]);
    setDraft("");
  };
  return (
    <div className="field">
      <div className="chain">
        {values.map((v) => (
          <span className="chip" key={v}>
            <span className="mono">{v}</span>
            <Button variant="ghost" style={{ height: 20 }} onClick={() => onChange(values.filter((x) => x !== v))}>
              ✕
            </Button>
          </span>
        ))}
        {values.length === 0 && <span className="mut">—</span>}
      </div>
      <div className="row">
        <input
          className="input mono"
          style={{ flex: 1 }}
          placeholder={placeholder}
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && add()}
        />
        <Button onClick={add}>
          + add
        </Button>
      </div>
    </div>
  );
}

export default function AgentEditorSurface() {
  const app = useApp();
  const nav = useNavigate();
  const qc = useQueryClient();
  const { ws: wsId = "default", id = "new" } = useParams();
  const isNew = id === "new";
  const [tab, setTab] = useState<Tab>("basics");
  const [cfg, setCfg] = useState<AgentConfig>(BLANK);
  const [dirty, setDirty] = useState(false);
  const [manageModels, setManageModels] = useState(false);
  const toast = useToast();
  useUnsavedGuard(dirty, app.t("You have unsaved changes. Leave anyway?", "有未保存的更改,仍要离开吗?"));

  // Existing agent: hydrate the draft from the config plane (managed object shape).
  const existing = useQuery({
    queryKey: ["config-agent", id],
    enabled: !isNew,
    queryFn: () => api.get<AgentConfigItem>(`/v1/config/agents/${id}`),
    retry: (n, err) => !isAbsent(err) && n < 2,
  });
  useEffect(() => {
    if (existing.data) {
      const { published: _published, ...rest } = existing.data;
      setCfg({ ...BLANK, ...rest });
      setDirty(false);
    }
  }, [existing.data]);

  const caps = useCapabilities();
  // Only models whose provider has a credential — a picked model always resolves a
  // real executor (never a run-time "no key" failure). `all` drives the hidden hint.
  const { ready: models, all: allModels } = useModels();
  const currentModel = modelId(cfg.model);
  // Keep the current selection visible even if it lost its credential (editing an
  // existing agent), flagged, so a save never silently drops the model.
  const modelOptions = useMemo(
    () => (currentModel && !models.includes(currentModel) ? [currentModel, ...models] : models),
    [currentModel, models],
  );

  const patch = (p: Partial<AgentConfig>) => {
    setCfg((c) => ({ ...c, ...p }));
    setDirty(true);
  };

  const targetId = () => (isNew ? cfg.id.trim() : id);
  const canSave = targetId().length > 0 && (cfg.system ?? "").trim().length > 0;
  const body = () => ({ ...cfg, id: targetId() });

  const validate = useMutation({
    mutationFn: () => api.post<{ valid: boolean; error?: string }>(`/v1/config/agents/${targetId()}/validate`, body()),
    onSuccess: (r) =>
      r.valid ? toast.ok(app.t("Config is valid.", "配置有效。")) : toast.err(r.error ?? "invalid"),
    onError: (e) => toast.err(e instanceof Error ? e.message : "error"),
  });
  // After a new agent's first save, navigate to its id URL — but only once `dirty`
  // has actually flushed to false, else the unsaved-guard (useBlocker(dirty)) would
  // swallow the navigation (the setDirty in onSuccess hasn't committed yet).
  const [savedId, setSavedId] = useState<string | null>(null);
  useEffect(() => {
    if (savedId && !dirty) {
      nav(`/w/${wsId}/agents/${savedId}`, { replace: true });
      setSavedId(null);
    }
  }, [savedId, dirty, wsId, nav]);

  const save = useMutation({
    mutationFn: () => api.put<{ id: string }>(`/v1/config/agents/${targetId()}`, body()),
    onSuccess: () => {
      setDirty(false);
      toast.ok(app.t("Saved.", "已保存。"));
      void qc.invalidateQueries({ queryKey: ["config-agents"] });
      if (isNew) setSavedId(targetId());
    },
    onError: (e) => toast.err(e instanceof Error ? e.message : "error"),
  });
  const publish = useMutation({
    mutationFn: () => api.post<PublishResult>(`/v1/config/agents/${targetId()}/publish`),
    onSuccess: (r) => {
      toast.ok(app.t(`Published · ${r.fingerprint.slice(0, 12)}`, `已发布 · ${r.fingerprint.slice(0, 12)}`));
      void qc.invalidateQueries({ queryKey: ["config-agents"] });
      void qc.invalidateQueries({ queryKey: ["config-agent", id] });
    },
    onError: (e) => toast.err(e instanceof Error ? e.message : "error"),
  });

  const TABS: { key: Tab; label: string; zh: string }[] = [
    { key: "basics", label: "Basics", zh: "基础" },
    { key: "context", label: "Context", zh: "上下文" },
    { key: "tools", label: "Tools", zh: "工具" },
    { key: "plugins", label: "Plugins & policy", zh: "插件与策略" },
    { key: "sandbox", label: "Sandbox", zh: "试运行" },
  ];

  return (
    <>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <span className="row">
          <Button variant="ghost" style={{ height: 26 }} onClick={() => nav(`/w/${wsId}/agents`)}>
            ← {app.t("Agents", "Agents")}
          </Button>
          <span className="crumb-title mono">{isNew ? app.t("new agent", "新建 agent") : id}</span>
          {dirty && <Pill tone="warn">{app.t("unsaved", "未保存")}</Pill>}
        </span>
        <span className="row">
          <Button variant="ghost" disabled={!canSave || validate.isPending} onClick={() => validate.mutate()}>
            {app.t("Validate", "校验")}
          </Button>
          <Button disabled={!canSave || save.isPending} onClick={() => save.mutate()}>
            {app.t("Save", "保存")}
          </Button>
          <Button
            variant="primary"
            disabled={!canSave || dirty || publish.isPending || isNew}
            title={dirty ? app.t("Save before publishing", "发布前请先保存") : ""}
            onClick={() => publish.mutate()}
          >
            {app.t("Publish", "发布")} ➤
          </Button>
        </span>
      </div>
      <div className="row">
        {TABS.map((t) => (
          <Button
            key={t.key}
            variant={tab === t.key ? "primary" : "ghost"}
            style={{ height: 26 }}
            onClick={() => setTab(t.key)}
          >
            {app.t(t.label, t.zh)}
          </Button>
        ))}
      </div>

      {tab === "sandbox" && (
        <SandboxPane agentId={id} ready={!isNew && !!existing.data?.published} dirty={dirty} />
      )}

      {tab !== "sandbox" && (
      <Card>
        {tab === "basics" && (
          <>
            <div className="row">
              <div className="field" style={{ flex: 1 }}>
                <label>{app.t("Agent id", "Agent id")}</label>
                <input
                  className="input mono"
                  value={cfg.id}
                  disabled={!isNew}
                  placeholder="coding-agent"
                  onChange={(e) => patch({ id: e.target.value })}
                />
              </div>
              <div className="field" style={{ flex: 1 }}>
                <label>{app.t("Name", "名称")}</label>
                <input className="input" value={cfg.name ?? ""} placeholder="Coding Assistant" onChange={(e) => patch({ name: e.target.value })} />
              </div>
            </div>
            <div className="field">
              <label className="row" style={{ justifyContent: "space-between" }}>
                <span>{app.t("Model (references workspace catalog)", "模型(引用工作区 catalog)")}</span>
                <button className="manage-link" onClick={() => setManageModels(true)}>
                  {app.t("Manage ↗", "管理 ↗")}
                </button>
              </label>
              {modelOptions.length > 0 ? (
                <select className="input mono" value={currentModel} onChange={(e) => patch({ model: { id: e.target.value } })}>
                  <option value="">{app.t("— select a model —", "— 选择模型 —")}</option>
                  {modelOptions.map((m) => (
                    <option key={m} value={m}>
                      {m}
                      {!models.includes(m) ? app.t("  ⚠ no credential", "  ⚠ 无凭证") : ""}
                    </option>
                  ))}
                </select>
              ) : (
                <div className="banner gate">
                  <span>◌</span>
                  <span>
                    {app.t(
                      "No model has a credential yet. Configure a provider + key in Models (Manage ↗) — only credentialed models can be selected.",
                      "还没有带凭证的模型。在 Models(管理 ↗)里配置一个 provider + key —— 只有带凭证的模型可选。",
                    )}
                  </span>
                </div>
              )}
              {allModels.length > models.length && (
                <span className="mut" style={{ fontSize: 12 }}>
                  {app.t(
                    `${allModels.length - models.length} model(s) hidden — no credential for their provider.`,
                    `${allModels.length - models.length} 个模型因缺凭证已隐藏。`,
                  )}
                </span>
              )}
            </div>
            <TextField
              label={app.t("Description", "描述")}
              value={cfg.description ?? ""}
              onChange={(e) => patch({ description: e.target.value })}
            />
            <TextField
              label={app.t("Max steps", "最大步数")}
              mono
              type="number"
              min={1}
              style={{ width: 140 }}
              value={cfg.max_steps}
              onChange={(e) => patch({ max_steps: Math.max(1, Number(e.target.value) || 1) })}
            />
            <TextAreaField
              label={app.t("System instructions", "系统指令")}
              mono
              rows={8}
              value={cfg.system ?? ""}
              onChange={(e) => patch({ system: e.target.value })}
            />
          </>
        )}

        {tab === "context" && (
          <>
            <div className="field">
              <label>{app.t("Context window policy", "上下文窗口策略")}</label>
              <div className="row">
                {(["keep_all", "keep_last"] as const).map((k) => (
                  <Button
                    key={k}
                    variant={cfg.context_policy.kind === k ? "primary" : "ghost"}
                    style={{ height: 26 }}
                    onClick={() =>
                      patch({
                        context_policy: (k === "keep_all" ? { kind: "keep_all" } : { kind: "keep_last", keep_last: 20 }) as ContextPolicy,
                      })
                    }
                  >
                    {k === "keep_all" ? app.t("Keep all", "全保留") : app.t("Keep last N", "保留最近 N")}
                  </Button>
                ))}
              </div>
            </div>
            {cfg.context_policy.kind === "keep_last" && (
              <TextField
                label={app.t("Keep last (non-system)", "保留最近条数")}
                mono
                type="number"
                min={0}
                style={{ width: 200 }}
                value={cfg.context_policy.keep_last}
                onChange={(e) => patch({ context_policy: { kind: "keep_last", keep_last: Math.max(0, Number(e.target.value) || 0) } })}
              />
            )}
            <span className="mut">
              {app.t(
                "Trims the model-visible transcript view; the committed history stays whole.",
                "只裁剪送模型的可见视图;已提交的历史保持完整。",
              )}
            </span>
          </>
        )}

        {tab === "tools" && (
          <div className="field">
            <label>{app.t("Tools", "工具")}</label>
            <span className="mut">
              {app.t(
                "Pick from the host's advertised tools, or add an id not in the catalog (e.g. an MCP tool).",
                "从 host 广告的工具中勾选,或添加目录外的 id(如 MCP 工具)。",
              )}
            </span>
            <CheckPicker
              options={[
                ...(caps.data?.tools ?? []).map((t) => ({ id: t.id, description: t.description })),
                // custom ids already on the agent but not in the catalog:
                ...cfg.tools
                  .filter((id) => !(caps.data?.tools ?? []).some((t) => t.id === id))
                  .map((id) => ({ id })),
              ]}
              selected={cfg.tools}
              onChange={(v) => patch({ tools: v })}
              empty={caps.isLoading ? "…" : app.t("No tools advertised.", "无广告工具。")}
            />
            <ListEditor
              values={cfg.tools}
              onChange={(v) => patch({ tools: v })}
              placeholder={app.t("add custom tool id (e.g. mcp__calc__add)", "添加自定义工具 id")}
            />
          </div>
        )}

        {tab === "plugins" && (
          <>
            <div className="field">
              <label>{app.t("Enabled plugins", "启用的插件")}</label>
              <span className="mut">
                {app.t("A runtime plugin contributes only when enabled here.", "运行时插件只有在此启用才生效。")}
              </span>
              <CheckPicker
                options={(caps.data?.plugins ?? []).map((p) => ({ id: p.id }))}
                selected={cfg.plugins}
                onChange={(v) => patch({ plugins: v })}
                empty={caps.isLoading ? "…" : app.t("No plugins available.", "无可用插件。")}
              />
            </div>
            <div className="field">
              <label>{app.t("Plugin config (policy sections)", "插件配置(策略段)")}</label>
              <span className="mut">
                {app.t(
                  "Per-plugin JSON: permission policy, state machine, deferred-tools, generative-UI all live here.",
                  "每插件 JSON:权限策略、状态机、deferred-tools、generative-UI 都在这里。",
                )}
              </span>
              <PluginConfigEditor
                pluginIds={cfg.plugins}
                config={cfg.plugin_config}
                schemas={Object.fromEntries(
                  (caps.data?.plugins ?? []).map((p) => [p.id, p.config_schema as JsonSchema]),
                )}
                onChange={(next) => patch({ plugin_config: next })}
              />
            </div>
            {(caps.data?.policies ?? []).some((p) => p.id === "permission") && (
              <div className="field">
                <label>{app.t("Permission policy", "权限策略")}</label>
                <span className="mut">
                  {app.t(
                    "Gate tool calls: a default decision plus ordered rules, enforced at runtime.",
                    "门禁工具调用:默认裁决 + 有序规则,运行时强制执行。",
                  )}
                </span>
                <PermissionEditor
                  value={(cfg.plugin_config.permission as PermissionConfig) ?? {}}
                  onChange={(v) => patch({ plugin_config: { ...cfg.plugin_config, permission: v } })}
                />
              </div>
            )}
          </>
        )}
      </Card>
      )}

      {manageModels && (
        <Drawer title={app.t("Models", "模型")} onClose={() => setManageModels(false)}>
          <ModelsSurface />
        </Drawer>
      )}
    </>
  );
}

/** Per-plugin config editor: a schema-driven form when the plugin advertises a
 * `config_schema` (from /v1/capabilities), else a validated JSON textarea. */
function PluginConfigEditor({
  pluginIds,
  config,
  schemas,
  onChange,
}: {
  pluginIds: string[];
  config: Record<string, unknown>;
  schemas: Record<string, JsonSchema>;
  onChange: (next: Record<string, unknown>) => void;
}) {
  const app = useApp();
  // Sections to show: every enabled plugin, plus any config key already present.
  const keys = Array.from(new Set([...pluginIds, ...Object.keys(config)]));
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [errs, setErrs] = useState<Record<string, string>>({});

  if (keys.length === 0) return <span className="mut">{app.t("No plugins enabled.", "未启用插件。")}</span>;

  const textFor = (k: string) => drafts[k] ?? JSON.stringify(config[k] ?? {}, null, 2);
  const commitJson = (k: string, raw: string) => {
    setDrafts((d) => ({ ...d, [k]: raw }));
    try {
      const parsed = raw.trim() === "" ? {} : JSON.parse(raw);
      setErrs((e) => ({ ...e, [k]: "" }));
      onChange({ ...config, [k]: parsed });
    } catch {
      setErrs((e) => ({ ...e, [k]: app.t("invalid JSON — not saved", "JSON 非法 — 未保存") }));
    }
  };

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 12 }}>
      {keys.map((k) => (
        <Card key={k} style={{ padding: "10px 12px" }}>
          <label className="row" style={{ justifyContent: "space-between", fontSize: 12, marginBottom: 6 }}>
            <span className="mono">{k}</span>
            {schemas[k] ? (
              <span className="mut" style={{ fontSize: 10 }}>{app.t("schema-driven", "schema 驱动")}</span>
            ) : (
              errs[k] && <span className="err" style={{ fontSize: 11 }}>{errs[k]}</span>
            )}
          </label>
          {schemas[k] ? (
            <SchemaForm
              schema={schemas[k]}
              value={config[k] ?? {}}
              onChange={(v) => onChange({ ...config, [k]: v })}
            />
          ) : (
            <textarea className="input mono" rows={5} value={textFor(k)} onChange={(e) => commitJson(k, e.target.value)} />
          )}
        </Card>
      ))}
    </div>
  );
}
