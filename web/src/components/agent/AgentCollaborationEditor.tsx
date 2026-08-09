import { useEffect, useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Link } from "react-router";
import type {
  Agent,
  AgentConfig,
  AgentConfigList,
  DelegationLimits,
  MultiagentTarget,
  Page,
} from "../../lib/api/types";
import { api, ws } from "../../lib/api/client";
import {
  DEFAULT_DELEGATION_LIMITS,
  AUXILIARY_PARENT_KEY,
  CONSERVATIVE_DELEGATION_LIMITS,
  agentTarget,
  delegateTargetView,
  withRoster,
} from "../../lib/agent-collaboration";
import { useApp } from "../../lib/app-state";
import { authoredAgents } from "../../lib/visible-agents";
import { Button, Card, Pill, SelectField, Switch, TextField } from "../ui";

const BALANCED: DelegationLimits = { max_depth: 3, max_parallel: 4, max_total: 16 };

function sameLimits(left: DelegationLimits, right: DelegationLimits): boolean {
  return left.max_depth === right.max_depth
    && left.max_parallel === right.max_parallel
    && left.max_total === right.max_total;
}

function targetId(target: MultiagentTarget, ownerId: string): string {
  return delegateTargetView(target, ownerId).id;
}

function isSelf(target: MultiagentTarget): boolean {
  return typeof target !== "string" && target.type === "self";
}

export default function AgentCollaborationEditor({
  config,
  onChange,
  onValidityChange,
}: {
  config: AgentConfig;
  onChange: (patch: Partial<AgentConfig>) => void;
  onValidityChange: (valid: boolean) => void;
}) {
  const app = useApp();
  const [candidateId, setCandidateId] = useState("");
  const authored = useQuery({
    queryKey: ["config-agents", app.workspaceId],
    queryFn: () => api.get<AgentConfigList>(ws("/v1/config/agents")),
  });
  const published = useQuery({
    queryKey: ["managed-agents", app.workspaceId],
    queryFn: () => api.get<Page<Agent>>(ws("/v1/agents?limit=100")),
  });
  const catalog = authoredAgents(authored.data?.data);
  const catalogById = useMemo(
    () => new Map(catalog.map((agent) => [agent.id, agent])),
    [catalog],
  );
  const publishedById = useMemo(
    () => new Map((published.data?.data ?? []).map((agent) => [agent.id, agent])),
    [published.data?.data],
  );
  const roster = config.multiagent?.agents ?? [];
  const delegates = roster.filter((target) => !isSelf(target));
  const recursive = roster.some(isSelf);
  const selectedIds = new Set(delegates.map((target) => targetId(target, config.id)));
  const available = catalog.filter((agent) =>
    agent.metadata?.[AUXILIARY_PARENT_KEY] === config.id
    && agent.published === true
    && !selectedIds.has(agent.id));
  const validationErrors = useMemo(() => {
    if (roster.length > 20) return [app.t("A coordinator can contain at most 20 entries.", "一个协调 Agent 最多包含 20 个条目。")];
    const seen = new Set<string>();
    const errors: string[] = [];
    for (const target of roster) {
      const view = delegateTargetView(target, config.id);
      // `{ type: "self" }` is a valid built-in auxiliary even before a new
      // primary Agent receives its id. Only external Agent references own ids.
      if (!view.recursiveSelf && !view.id.trim()) errors.push(app.t("Every auxiliary Agent needs an id.", "每个辅助 Agent 都需要 ID。"));
      if (!view.recursiveSelf && view.id === config.id) errors.push(app.t("Use the recursive switch instead of selecting this Agent directly.", "请使用递归开关，不要直接选择当前 Agent。"));
      if (view.version !== undefined && view.version < 1) errors.push(app.t("Pinned versions must be at least 1.", "固定版本必须至少为 1。"));
      if (!view.recursiveSelf) {
        if (seen.has(view.id)) errors.push(app.t(`Agent ${view.id} is duplicated.`, `Agent ${view.id} 重复。`));
        seen.add(view.id);
      }
    }
    return errors;
  }, [app, config.id, roster]);

  useEffect(() => {
    onValidityChange(validationErrors.length === 0);
  }, [onValidityChange, validationErrors.length]);

  const replaceDelegate = (index: number, replacement: MultiagentTarget) => {
    let delegateIndex = -1;
    const next = roster.map((target) => {
      if (isSelf(target)) return target;
      delegateIndex += 1;
      return delegateIndex === index ? replacement : target;
    });
    onChange({ multiagent: withRoster(next) });
  };
  const removeDelegate = (index: number) => {
    let delegateIndex = -1;
    const next = roster.filter((target) => {
      if (isSelf(target)) return true;
      delegateIndex += 1;
      return delegateIndex !== index;
    });
    onChange({ multiagent: withRoster(next) });
  };
  const addDelegate = () => {
    if (!candidateId || roster.length >= 20) return;
    const next = [...delegates, agentTarget(candidateId), ...(recursive ? [{ type: "self" } as const] : [])];
    onChange({
      multiagent: withRoster(next),
      ...(config.multiagent == null && config.delegation_limits == null
        ? { delegation_limits: CONSERVATIVE_DELEGATION_LIMITS }
        : {}),
    });
    setCandidateId("");
  };
  const setRecursive = (enabled: boolean) => {
    const next = enabled
      ? [...delegates, { type: "self" } as const]
      : delegates;
    onChange({
      multiagent: withRoster(next),
      ...(enabled && config.multiagent == null && config.delegation_limits == null
        ? { delegation_limits: CONSERVATIVE_DELEGATION_LIMITS }
        : {}),
    });
  };

  const limits = config.delegation_limits ?? DEFAULT_DELEGATION_LIMITS;
  const profile = config.delegation_limits == null
    ? "system"
    : sameLimits(limits, CONSERVATIVE_DELEGATION_LIMITS)
      ? "conservative"
      : sameLimits(limits, BALANCED)
        ? "balanced"
        : "custom";
  const setLimit = (key: keyof DelegationLimits, value: number) => {
    onChange({ delegation_limits: { ...limits, [key]: Math.max(1, Math.floor(value || 1)) } });
  };

  return (
    <Card className="agent-config-card collaboration-editor">
      <div className="row collaboration-card-head">
        <div>
          <h2>{app.t("Auxiliary Agents", "辅助 Agent")}</h2>
          <p className="hint">{app.t(
            "Every primary Agent has a built-in auxiliary that inherits its configuration. Add parent-owned specialists here when they need different instructions or models.",
            "每个主 Agent 默认包含一个继承自身配置的内置辅助 Agent。需要不同提示词或模型时，可在这里添加归属于当前主 Agent 的专属辅助 Agent。",
          )}</p>
        </div>
        <Pill tone={roster.length > 0 ? "agent" : "neutral"}>
          {roster.length > 0 ? app.t(`${roster.length} entries`, `${roster.length} 个条目`) : app.t("disabled", "未启用")}
        </Pill>
      </div>

      {delegates.length > 0 && (
        <div role="list" aria-label={app.t("Auxiliary Agent roster", "辅助 Agent 清单")} className="delegate-roster">
          {delegates.map((target, index) => {
            const view = delegateTargetView(target, config.id);
            const item = catalogById.get(view.id);
            const release = publishedById.get(view.id);
            const pinned = view.version !== undefined;
            return (
              <div role="listitem" className="delegate-row" key={`${view.id}-${index}`}>
                <div className="delegate-identity">
                  <strong>{item?.name || release?.name || view.id}</strong>
                  <span className="mono mut">{view.id}</span>
                  <span className="row" style={{ gap: 6 }}>
                    <Pill tone={item?.published ? "ok" : item ? "warn" : "danger"}>
                      {item?.published
                        ? app.t("published", "已发布")
                        : item
                          ? app.t("draft only", "仅草稿")
                          : app.t("missing", "缺失")}
                    </Pill>
                    {release?.version !== undefined && <span className="mut mono">v{release.version}</span>}
                  </span>
                </div>
                <SelectField
                  label={app.t("Version policy", "版本策略")}
                  value={pinned ? "pinned" : "publish-time"}
                  onChange={(event) => replaceDelegate(index, event.target.value === "pinned"
                    ? agentTarget(view.id, release?.version ?? 1)
                    : agentTarget(view.id))}
                >
                  <option value="publish-time">{app.t("Resolve when parent publishes", "发布主 Agent 时解析")}</option>
                  <option value="pinned">{app.t("Pin a version", "固定版本")}</option>
                </SelectField>
                {pinned && (
                  <TextField
                    label={app.t("Published version", "发布版本")}
                    aria-label={`${app.t("Published version", "发布版本")} ${index + 1}`}
                    type="number"
                    min={1}
                    value={view.version}
                    onChange={(event) => replaceDelegate(index, agentTarget(view.id, Math.max(1, Number(event.target.value) || 1)))}
                  />
                )}
                <Button
                  variant="ghost"
                  aria-label={app.t(`Remove ${view.id}`, `移除 ${view.id}`)}
                  onClick={() => removeDelegate(index)}
                >✕</Button>
              </div>
            );
          })}
        </div>
      )}

      <div className="delegate-add-row">
        <SelectField
          label={app.t("Add a published specialist", "添加已发布的专属辅助 Agent")}
          value={candidateId}
          disabled={roster.length >= 20}
          onChange={(event) => setCandidateId(event.target.value)}
        >
          <option value="">{authored.isLoading
            ? app.t("Loading Agents…", "正在加载 Agent…")
            : app.t("Choose an attached specialist…", "选择专属辅助 Agent…")}</option>
          {available.map((agent) => (
            <option key={agent.id} value={agent.id}>{agent.name || agent.id} · {agent.id}</option>
          ))}
        </SelectField>
        <Button disabled={!candidateId || roster.length >= 20} onClick={addDelegate}>
          + {app.t("Add", "添加")}
        </Button>
        {config.id.trim() && config.generation !== undefined && (
          <Link className="btn" to={`/w/${app.workspaceId}/agents/new?parent=${encodeURIComponent(config.id.trim())}`}>
            + {app.t("New specialist", "新建专属辅助 Agent")}
          </Link>
        )}
        <Link className="mut" to={`/w/${app.workspaceId}/agents?view=collaborations`}>
          {app.t("View all relationships ↗", "查看全部关系 ↗")}
        </Link>
      </div>
      {authored.error instanceof Error && <div className="err">{app.t("Agent catalog could not load.", "无法加载 Agent 目录。")} {authored.error.message}</div>}
      {!authored.isLoading && available.length === 0 && delegates.length === 0 && (
        <div className="banner info">
          <span>ⓘ</span>
          <span>{app.t(
            "The built-in auxiliary is ready. Create a specialist here only when it needs a different model, prompt, tools, or permissions.",
            "内置辅助 Agent 已可使用。仅在需要不同模型、提示词、工具或权限时，再创建专属辅助 Agent。",
          )}</span>
        </div>
      )}

      <div className="recursive-control">
        <div>
          <strong>{app.t("Built-in auxiliary Agent", "内置辅助 Agent")}</strong>
          <p className="hint">{app.t(
            "It inherits this primary Agent's published configuration and runs as a child. Safety limits prevent unbounded recursion.",
            "它继承当前主 Agent 的已发布配置，并作为子运行执行；安全预算会阻止无限递归。",
          )}</p>
        </div>
        <Switch
          aria-label={app.t("Built-in auxiliary Agent", "内置辅助 Agent")}
          checked={recursive}
          onChange={(event) => setRecursive(event.target.checked)}
        />
      </div>

      <div className="delegation-budget">
        <div>
          <h3>{app.t("Delegation safety budget", "委托安全预算")}</h3>
          <p className="hint">{app.t(
            "These limits bound recursive depth, simultaneous child runs, and total delegations in one run.",
            "这些限制约束单次运行中的递归层级、并行子运行数和委托总次数。",
          )}</p>
        </div>
        <SelectField
          label={app.t("Budget profile", "预算方案")}
          value={profile}
          onChange={(event) => {
            if (event.target.value === "system") onChange({ delegation_limits: undefined });
            else if (event.target.value === "conservative") onChange({ delegation_limits: CONSERVATIVE_DELEGATION_LIMITS });
            else if (event.target.value === "balanced") onChange({ delegation_limits: BALANCED });
            else onChange({ delegation_limits: { ...limits } });
          }}
        >
          <option value="system">{app.t("System default · 8 / 8 / 64", "系统默认 · 8 / 8 / 64")}</option>
          <option value="conservative">{app.t("Conservative · 2 / 2 / 8 (recommended)", "保守 · 2 / 2 / 8（推荐）")}</option>
          <option value="balanced">{app.t("Balanced · 3 / 4 / 16", "均衡 · 3 / 4 / 16")}</option>
          <option value="custom">{app.t("Custom", "自定义")}</option>
        </SelectField>
        <div className="delegation-limit-fields">
          <TextField
            label={app.t("Maximum depth", "最大委托层级")}
            type="number"
            min={1}
            value={limits.max_depth}
            onChange={(event) => setLimit("max_depth", Number(event.target.value))}
          />
          <TextField
            label={app.t("Maximum parallel", "最大并行辅助 Agent")}
            type="number"
            min={1}
            value={limits.max_parallel}
            onChange={(event) => setLimit("max_parallel", Number(event.target.value))}
          />
          <TextField
            label={app.t("Maximum total", "单次运行最多委托次数")}
            type="number"
            min={1}
            value={limits.max_total}
            onChange={(event) => setLimit("max_total", Number(event.target.value))}
          />
        </div>
      </div>

      {validationErrors.map((error) => <div role="alert" className="err" key={error}>{error}</div>)}
      {delegates.some((target) => catalogById.get(targetId(target, config.id))?.published !== true) && (
        <div className="banner warn">
          <span>!</span>
          <span>{app.t(
            "One or more referenced Agents are missing or not published. The existing reference is preserved, but publish validation may reject it.",
            "一个或多个被引用的 Agent 缺失或尚未发布。现有引用会保留，但发布校验可能拒绝它。",
          )}</span>
        </div>
      )}
    </Card>
  );
}
