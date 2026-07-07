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
import { api, isAbsent } from "../lib/api/client";
import type { AgentConfig, ContextPolicy, ProviderCatalog, PublishResult } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { useUnsavedGuard } from "../lib/useUnsavedGuard";
import ModelsSurface from "./models";

type Tab = "basics" | "context" | "tools" | "plugins";

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
            <button className="btn ghost" style={{ height: 20 }} onClick={() => onChange(values.filter((x) => x !== v))}>
              ✕
            </button>
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
        <button className="btn" onClick={add}>
          + add
        </button>
      </div>
    </div>
  );
}

export default function AgentEditorSurface() {
  const app = useApp();
  const nav = useNavigate();
  const qc = useQueryClient();
  const { pid = "", id = "new" } = useParams();
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
    queryFn: () => api.get<AgentConfig>(`/v1/config/agents/${id}`),
    retry: (n, err) => !isAbsent(err) && n < 2,
  });
  useEffect(() => {
    if (existing.data) {
      setCfg({ ...BLANK, ...existing.data });
      setDirty(false);
    }
  }, [existing.data]);

  const catalog = useQuery({
    queryKey: ["catalog"],
    queryFn: () => api.get<ProviderCatalog>("/v1/config/catalog"),
  });
  const models = useMemo(
    () => Array.from(new Set((catalog.data?.offerings ?? []).map((o) => o.model_id))),
    [catalog.data],
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
  const save = useMutation({
    mutationFn: () => api.put<{ id: string }>(`/v1/config/agents/${targetId()}`, body()),
    onSuccess: () => {
      setDirty(false);
      toast.ok(app.t("Saved.", "已保存。"));
      void qc.invalidateQueries({ queryKey: ["config-agents"] });
      if (isNew) nav(`/p/${pid}/agents/${targetId()}`, { replace: true });
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
  ];

  return (
    <>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <span className="row">
          <button className="btn ghost" style={{ height: 26 }} onClick={() => nav(`/p/${pid}/agents`)}>
            ← {app.t("Agents", "Agents")}
          </button>
          <span className="crumb-title mono">{isNew ? app.t("new agent", "新建 agent") : id}</span>
          {dirty && <span className="pill warn">{app.t("unsaved", "未保存")}</span>}
        </span>
        <span className="row">
          <button className="btn ghost" disabled={!canSave || validate.isPending} onClick={() => validate.mutate()}>
            {app.t("Validate", "校验")}
          </button>
          <button className="btn" disabled={!canSave || save.isPending} onClick={() => save.mutate()}>
            {app.t("Save", "保存")}
          </button>
          <button
            className="btn primary"
            disabled={!canSave || dirty || publish.isPending || isNew}
            title={dirty ? app.t("Save before publishing", "发布前请先保存") : ""}
            onClick={() => publish.mutate()}
          >
            {app.t("Publish", "发布")} ➤
          </button>
        </span>
      </div>
      <div className="row">
        {TABS.map((t) => (
          <button
            key={t.key}
            className={`btn ${tab === t.key ? "primary" : "ghost"}`}
            style={{ height: 26 }}
            onClick={() => setTab(t.key)}
          >
            {app.t(t.label, t.zh)}
          </button>
        ))}
      </div>

      <div className="card">
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
              {models.length > 0 ? (
                <select className="input mono" value={modelId(cfg.model)} onChange={(e) => patch({ model: { id: e.target.value } })}>
                  <option value="">{app.t("— select a model —", "— 选择模型 —")}</option>
                  {models.map((m) => (
                    <option key={m} value={m}>
                      {m}
                    </option>
                  ))}
                </select>
              ) : (
                <input className="input mono" value={modelId(cfg.model)} placeholder="kimi-k2" onChange={(e) => patch({ model: { id: e.target.value } })} />
              )}
            </div>
            <div className="field">
              <label>{app.t("Description", "描述")}</label>
              <input className="input" value={cfg.description ?? ""} onChange={(e) => patch({ description: e.target.value })} />
            </div>
            <div className="field" style={{ width: 140 }}>
              <label>{app.t("Max steps", "最大步数")}</label>
              <input
                className="input mono"
                type="number"
                min={1}
                value={cfg.max_steps}
                onChange={(e) => patch({ max_steps: Math.max(1, Number(e.target.value) || 1) })}
              />
            </div>
            <div className="field">
              <label>{app.t("System instructions", "系统指令")}</label>
              <textarea className="input mono" rows={8} value={cfg.system ?? ""} onChange={(e) => patch({ system: e.target.value })} />
            </div>
          </>
        )}

        {tab === "context" && (
          <>
            <div className="field">
              <label>{app.t("Context window policy", "上下文窗口策略")}</label>
              <div className="row">
                {(["keep_all", "keep_last"] as const).map((k) => (
                  <button
                    key={k}
                    className={`btn ${cfg.context_policy.kind === k ? "primary" : "ghost"}`}
                    style={{ height: 26 }}
                    onClick={() =>
                      patch({
                        context_policy: (k === "keep_all" ? { kind: "keep_all" } : { kind: "keep_last", keep_last: 20 }) as ContextPolicy,
                      })
                    }
                  >
                    {k === "keep_all" ? app.t("Keep all", "全保留") : app.t("Keep last N", "保留最近 N")}
                  </button>
                ))}
              </div>
            </div>
            {cfg.context_policy.kind === "keep_last" && (
              <div className="field" style={{ width: 200 }}>
                <label>{app.t("Keep last (non-system)", "保留最近条数")}</label>
                <input
                  className="input mono"
                  type="number"
                  min={0}
                  value={cfg.context_policy.keep_last}
                  onChange={(e) => patch({ context_policy: { kind: "keep_last", keep_last: Math.max(0, Number(e.target.value) || 0) } })}
                />
              </div>
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
              {app.t("Hand tools bound by id at compile. Empty = no hand tools.", "编译时按 id 绑定的 hand 工具;空 = 无。")}
            </span>
            <ListEditor values={cfg.tools} onChange={(v) => patch({ tools: v })} placeholder="mcp__calc__add" />
          </div>
        )}

        {tab === "plugins" && (
          <>
            <div className="field">
              <label>{app.t("Enabled plugins", "启用的插件")}</label>
              <span className="mut">
                {app.t("A runtime plugin contributes only when listed here.", "运行时插件只有列在此处才生效。")}
              </span>
              <ListEditor values={cfg.plugins} onChange={(v) => patch({ plugins: v })} placeholder="permission" />
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
                onChange={(next) => patch({ plugin_config: next })}
              />
            </div>
          </>
        )}
      </div>

      {manageModels && (
        <Drawer title={app.t("Models", "模型")} onClose={() => setManageModels(false)}>
          <ModelsSurface />
        </Drawer>
      )}
    </>
  );
}

/** A JSON section editor keyed by plugin id — the honest general form for any
 * plugin's config (specialised visual builders can graft on later, keyed the same). */
function PluginConfigEditor({
  pluginIds,
  config,
  onChange,
}: {
  pluginIds: string[];
  config: Record<string, unknown>;
  onChange: (next: Record<string, unknown>) => void;
}) {
  const app = useApp();
  // Sections to show: every enabled plugin, plus any config key already present.
  const keys = Array.from(new Set([...pluginIds, ...Object.keys(config)]));
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [errs, setErrs] = useState<Record<string, string>>({});

  if (keys.length === 0) return <span className="mut">{app.t("No plugins enabled.", "未启用插件。")}</span>;

  const textFor = (k: string) => drafts[k] ?? JSON.stringify(config[k] ?? {}, null, 2);
  const commit = (k: string, raw: string) => {
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
    <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
      {keys.map((k) => (
        <div key={k}>
          <label className="row" style={{ justifyContent: "space-between", fontSize: 12 }}>
            <span className="mono">{k}</span>
            {errs[k] && <span className="err" style={{ fontSize: 11 }}>{errs[k]}</span>}
          </label>
          <textarea
            className="input mono"
            rows={5}
            value={textFor(k)}
            onChange={(e) => commit(k, e.target.value)}
          />
        </div>
      ))}
    </div>
  );
}
