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
import Modal from "../components/ui/Modal";
import ConfigDiff from "../components/agent/ConfigDiff";
import { useToast } from "../components/ui/Toast";
import { Button, Card, CheckPicker, Pill, Segmented, TextAreaField, TextField } from "../components/ui";
import type { JsonSchema } from "../components/ui";
import BehaviorCard from "../components/agent/BehaviorCard";
import ToolOverridesEditor from "../components/agent/ToolOverridesEditor";
import SandboxPane from "../components/session/SandboxPane";
import PermissionEditor from "../components/agent/PermissionEditor";
import ResourcesTab from "../components/agent/ResourcesTab";
import { api, isAbsent } from "../lib/api/client";
import type {
  AgentConfig,
  AgentConfigItem,
  ContextPolicy,
  PermissionConfig,
  PublishResult,
  ValidationIssue,
  ValidationResult,
} from "../lib/api/types";
import { labelForPath, sectionForPath } from "../lib/config-diff";
import { useApp } from "../lib/app-state";
import { useCapabilities } from "../lib/useCapabilities";
import { useModels } from "../lib/useModels";
import { useUnsavedGuard } from "../lib/useUnsavedGuard";
import ModelsSurface from "./models";

// Editor sections, organized by user intent (not by mechanism): Behavior groups the
// runtime behaviors (context window, auto-compaction, memory recall, tool ordering) as
// named cards; Tools holds selection + presentation + permissions.
type Tab = "overview" | "behavior" | "tools" | "resources";

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
  const [tab, setTab] = useState<Tab>("overview");
  // "Try it" opens the sandbox as a slide-over from any section, so you can tweak → test
  // without leaving your place.
  const [showSandbox, setShowSandbox] = useState(false);
  // Publish opens a confirm modal previewing the diff vs the config as loaded.
  const [showPublish, setShowPublish] = useState(false);
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
  // The config as loaded when the editor opened (the detail query is not refetched on
  // save), so the publish preview diffs the outgoing config against the pre-session state.
  const baseline = useMemo(() => {
    if (!existing.data) return {};
    const { published: _published, ...rest } = existing.data;
    return rest;
  }, [existing.data]);

  const patch = (p: Partial<AgentConfig>) => {
    setCfg((c) => ({ ...c, ...p }));
    setDirty(true);
    if (issues.length) setIssues([]); // an edit invalidates the previous validation

  };

  const targetId = () => (isNew ? cfg.id.trim() : id);
  const canSave = targetId().length > 0 && (cfg.system ?? "").trim().length > 0;
  const body = () => ({ ...cfg, id: targetId() });

  // Validation issues from the config domain (compile), field-routed. The UI only
  // projects them — it never re-derives a rule (that truth lives in compile).
  const [issues, setIssues] = useState<ValidationIssue[]>([]);
  const validate = useMutation({
    mutationFn: () => api.post<ValidationResult>(`/v1/config/agents/${targetId()}/validate`, body()),
    onSuccess: (r) => {
      setIssues(r.issues ?? []);
      if (r.valid) toast.ok(app.t("Config is valid.", "配置有效。"));
      else toast.err(app.t("Config has issues — see below.", "配置有问题 — 见下方。"));
    },
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

  const SECTIONS: { key: Tab; label: string; zh: string }[] = [
    { key: "overview", label: "Overview", zh: "概览" },
    { key: "behavior", label: "Behavior", zh: "行为" },
    { key: "tools", label: "Tools", zh: "工具" },
    { key: "resources", label: "Resources", zh: "资源" },
  ];
  // The count shown as a rail badge, so each section's fill is visible at a glance.
  const sectionBadge = (k: Tab) =>
    k === "behavior" ? cfg.plugins.length : k === "tools" ? cfg.tools.length : 0;

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
            onClick={() => setShowPublish(true)}
          >
            {app.t("Publish", "发布")} ➤
          </Button>
        </span>
      </div>
      <div className="agent-editor">
        {/* Left rail: sections by intent (Overview / Behavior / Tools / Resources). */}
        <div className="editor-rail" role="tablist" aria-label={app.t("Agent config sections", "Agent 配置分区")}>
          {SECTIONS.map((s) => {
            const n = sectionBadge(s.key);
            const hasIssue = issues.some((i) => sectionForPath(i.path) === s.key);
            return (
              <Button
                key={s.key}
                role="tab"
                aria-selected={tab === s.key}
                variant={tab === s.key ? "primary" : "ghost"}
                onClick={() => setTab(s.key)}
              >
                <span>
                  {app.t(s.label, s.zh)}
                  {hasIssue && <span title={app.t("has a validation issue", "有校验问题")} style={{ color: "var(--danger)", marginLeft: 4 }}>●</span>}
                </span>
                {n > 0 && <span className="rail-badge">{n}</span>}
              </Button>
            );
          })}
          {/* Try it: opens the sandbox as a slide-over (available from any section). */}
          <Button variant="ghost" style={{ marginTop: 8, justifyContent: "center" }} onClick={() => setShowSandbox(true)}>
            ▷ {app.t("Try it", "试运行")}
          </Button>
        </div>
        {/* Content column: one section at a time. */}
        <div className="editor-content">

      {issues.length > 0 && (
        <div className="banner warn" style={{ flexDirection: "column", alignItems: "stretch", gap: 6 }}>
          {issues.map((iss, i) => (
            <div key={i} className="row" style={{ justifyContent: "space-between", gap: 8 }}>
              <span>
                <strong>{labelForPath(iss.path) || app.t("Config", "配置")}</strong>
                {" — "}
                {iss.message}
              </span>
              <Button variant="ghost" style={{ height: 24 }} onClick={() => setTab(sectionForPath(iss.path))}>
                {app.t("Go to section →", "前往该项 →")}
              </Button>
            </div>
          ))}
        </div>
      )}

      {tab === "resources" && (
        <Card>
          {isNew ? (
            <div className="banner gate">
              <span>◌</span>
              <span>{app.t("Save the agent first, then bind resources to it.", "先保存 agent,再给它绑定资源。")}</span>
            </div>
          ) : (
            <ResourcesTab agentId={id} />
          )}
        </Card>
      )}

      {tab !== "resources" && (
      <Card>
        {tab === "overview" && (
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
              hint={app.t(
                "Shown to other agents / the orchestrator for delegation — what this agent does. Not part of the system prompt; editing it never republishes the agent.",
                "给其他 agent / 编排器看,用于委派——说明这个 agent 做什么。不进系统提示;改它不会重新发布 agent。",
              )}
              value={cfg.description ?? ""}
              placeholder={app.t("Researches and summarizes technical docs", "检索并综述技术文档")}
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
              hint={app.t(
                "The agent's own behavior — the system prompt it runs with. Bound-resource guidance is appended automatically at publish (see the Resources tab).",
                "agent 自己的行为——它运行时的系统提示。绑定资源的说明会在发布时自动追加(见资源标签页)。",
              )}
              mono
              rows={8}
              value={cfg.system ?? ""}
              onChange={(e) => patch({ system: e.target.value })}
            />
          </>
        )}

        {tab === "behavior" && (
          <>
            <div className="field">
              <label>{app.t("Context window policy", "上下文窗口策略")}</label>
              <Segmented
                options={[
                  { value: "keep_all", label: app.t("Keep all", "全保留") },
                  { value: "keep_last", label: app.t("Keep last N", "保留最近 N") },
                ]}
                value={cfg.context_policy.kind}
                onChange={(k) =>
                  patch({
                    context_policy: (k === "keep_all" ? { kind: "keep_all" } : { kind: "keep_last", keep_last: 20 }) as ContextPolicy,
                  })
                }
              />
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
            <div className="field" style={{ marginTop: 16 }}>
              <label>{app.t("Runtime behaviors", "运行时行为")}</label>
              <span className="mut">
                {app.t(
                  "Each behavior runs automatically when enabled — toggle it on and tune it. Advanced fields are one click away.",
                  "每个行为启用后自动生效——打开开关并微调。高级字段一键展开。",
                )}
              </span>
              <div style={{ display: "flex", flexDirection: "column", gap: 10, marginTop: 8 }}>
                {(caps.data?.plugins ?? []).map((p) => (
                  <BehaviorCard
                    key={p.id}
                    id={p.id}
                    schema={p.config_schema as JsonSchema | undefined}
                    enabled={cfg.plugins.includes(p.id)}
                    config={(cfg.plugin_config[p.id] as Record<string, unknown>) ?? {}}
                    onToggle={(on) => {
                      if (on) {
                        patch({ plugins: [...cfg.plugins, p.id] });
                      } else {
                        // Disabling drops the plugin AND its config section, so the saved
                        // config carries no orphaned `plugin_config` for an inactive plugin.
                        const { [p.id]: _removed, ...rest } = cfg.plugin_config;
                        patch({ plugins: cfg.plugins.filter((x) => x !== p.id), plugin_config: rest });
                      }
                    }}
                    onConfig={(v) => patch({ plugin_config: { ...cfg.plugin_config, [p.id]: v } })}
                  />
                ))}
                {(caps.data?.plugins ?? []).length === 0 && (
                  <span className="mut">{caps.isLoading ? "…" : app.t("No behaviors available.", "无可用行为。")}</span>
                )}
              </div>
            </div>
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
            <div className="field" style={{ marginTop: 14 }}>
              <label>{app.t("Tool presentation", "工具呈现")}</label>
              <span className="mut">
                {app.t(
                  "Rename a tool for the model (alias), override its description, or defer it (its schema is sent only after the model loads it) — works for MCP tools too. Target is the tool's id.",
                  "给模型改工具名(别名)、覆盖描述,或延迟加载(模型加载后才发 schema)——对 MCP 工具同样适用。目标是工具 id。",
                )}
              </span>
              <ToolOverridesEditor
                tools={cfg.tools}
                value={cfg.tool_overrides ?? []}
                onChange={(v) => patch({ tool_overrides: v })}
              />
            </div>
            {(caps.data?.policies ?? []).some((p) => p.id === "permission") && (
              <div className="field" style={{ marginTop: 16 }}>
                <label>{app.t("Permissions", "权限")}</label>
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
          </div>
        )}

      </Card>
      )}
        </div>
      </div>

      {manageModels && (
        <Drawer title={app.t("Models", "模型")} onClose={() => setManageModels(false)}>
          <ModelsSurface />
        </Drawer>
      )}

      {showSandbox && (
        <Drawer title={app.t("Try it", "试运行")} onClose={() => setShowSandbox(false)}>
          <SandboxPane agentId={id} ready={!isNew && !!existing.data?.published} dirty={dirty} />
        </Drawer>
      )}

      {showPublish && (
        <Modal
          title={app.t("Publish changes?", "发布改动?")}
          onClose={() => setShowPublish(false)}
          footer={
            <>
              <button className="btn" onClick={() => setShowPublish(false)}>
                {app.t("Cancel", "取消")}
              </button>
              <button
                className="btn primary"
                disabled={publish.isPending}
                onClick={() => {
                  publish.mutate();
                  setShowPublish(false);
                }}
              >
                {app.t("Publish", "发布")} ➤
              </button>
            </>
          }
        >
          <p className="mut" style={{ marginTop: 0 }}>
            {app.t(
              "These changes will compile and go live for new runs of this agent:",
              "以下改动将编译并对该 agent 的新运行生效:",
            )}
          </p>
          <ConfigDiff before={baseline} after={body()} />
        </Modal>
      )}
    </>
  );
}

