import type { AgentConfig, AgentToolsetMemberCap, InputBinding, PluginCap, RuntimeCap } from "../../lib/api/types";
import { DEFAULT_DELEGATION_LIMITS, rosterOf } from "../../lib/agent-collaboration";
import { executionRuntimeId } from "../../lib/agent-model-selection";
import { effectiveAgentToolPermission, selectedAgentToolIds } from "../../lib/agent-toolsets";
import { runtimeToolCompatibility } from "../../lib/runtime-tool-compatibility";
import { useApp } from "../../lib/app-state";
import { Pill } from "../ui";
import { webSearchProviderOptions } from "./WebSearchBehaviorEditor";

export default function PublicationSnapshotSummary({
  sourceRevision,
  resourceRevision,
  resources,
  config,
  runtimes = [],
  plugins = [],
  toolMembers = [],
}: {
  sourceRevision?: number;
  resourceRevision: number;
  resources: InputBinding[];
  config?: AgentConfig;
  runtimes?: RuntimeCap[];
  plugins?: PluginCap[];
  toolMembers?: AgentToolsetMemberCap[];
}) {
  const app = useApp();
  const delegates = config ? rosterOf(config) : [];
  const limits = config?.delegation_limits ?? DEFAULT_DELEGATION_LIMITS;
  const runtimeId = config ? executionRuntimeId(config.model) : "awaken";
  const runtime = runtimes.find((candidate) => candidate.id === runtimeId);
  const tools = config ? selectedAgentToolIds(config.tools, toolMembers) : [];
  const realizationFor = (toolId: string) => {
    const plugin = plugins.find((candidate) => candidate.id === toolId);
    const providerId = (config?.plugin_config[toolId] as { provider_id?: string } | undefined)?.provider_id;
    return webSearchProviderOptions(plugin?.config_schema)
      .find((provider) => provider.id === providerId)?.realization ?? "host_executed";
  };
  const supportLabel = (support: "supported" | "conditional" | "unavailable") => support === "supported"
    ? app.t("Supported", "支持")
    : support === "conditional"
      ? app.t("Needs verification", "需要验证")
      : app.t("Unavailable", "不可用");

  return (
    <div style={{ marginTop: 16, paddingTop: 14, borderTop: "1px solid var(--line)" }}>
      <div className="row" style={{ justifyContent: "space-between", alignItems: "baseline" }}>
        <strong>{app.t("Version used by new Sessions", "新 Session 使用的版本")}</strong>
        <span className="mut mono" style={{ fontSize: 11 }}>
          {app.t("Agent revision", "Agent 版本")} {sourceRevision ?? "—"}
          {" · "}
          {app.t("Resource revision", "资源版本")} {resourceRevision}
        </span>
      </div>
      <p className="mut" style={{ margin: "6px 0 10px" }}>
        {app.t(
          "New Sessions keep using this reviewed Agent version and these Resource bindings, even if you edit the draft later.",
          "新的 Session 会持续使用当前审阅的 Agent 版本和这些资源绑定；之后修改草稿不会影响它们。",
        )}
      </p>
      {config && (
        <div className="publication-execution-plan">
          <div className="row" style={{ justifyContent: "space-between", flexWrap: "wrap" }}>
            <strong>{app.t("Execution plan", "执行计划")}</strong>
            <span className="mut mono">
              {runtime?.label ?? app.t("Native Awaken", "Native Awaken")}
              {runtime?.local?.version ? ` · ${runtime.local.version}` : ""}
            </span>
          </div>
          <div className="publication-runtime-facts">
            <span>{app.t("Working directory", "工作目录")} <b className="mono">{typeof config.model === "object" && "configuration" in config.model && config.model.configuration?.working_directory ? `/${config.model.configuration.working_directory}` : "/"}</b></span>
            <span>{app.t("Tool bridge", "工具桥")} <Pill tone={runtimeId === "awaken" || runtime?.features?.awaken_tool_bridge === "supported" ? "ok" : "warn"}>{runtimeId === "awaken" ? app.t("Native", "原生") : supportLabel(runtime?.features?.awaken_tool_bridge ?? "conditional")}</Pill></span>
          </div>
          {tools.length === 0 ? <p className="mut">{app.t("No executable tools selected.", "未选择可执行工具。")}</p> : (
            <div className="publication-tool-plan" role="list" aria-label={app.t("Published tool execution plan", "发布工具执行计划")}>
              {tools.map((toolId) => {
                const compatibility = runtimeToolCompatibility(config.model, runtime, realizationFor(toolId));
                const owner = compatibility.owner === "model_provider"
                  ? app.t("Model provider", "模型供应商")
                  : compatibility.owner === "awaken_bridge"
                    ? app.t("Awaken via ACP bridge", "Awaken 通过 ACP 桥")
                    : app.t("Native Awaken", "Native Awaken");
                const permission = effectiveAgentToolPermission(config.tools, toolId).type;
                return <div role="listitem" className="publication-tool-row" key={toolId}>
                  <strong className="mono">{toolId}</strong>
                  <span>{owner}</span>
                  <Pill tone={compatibility.support === "supported" ? "ok" : "warn"}>{supportLabel(compatibility.support)}</Pill>
                  <Pill tone={permission === "always_ask" && compatibility.approval === "supported" ? "warn" : "neutral"}>
                    {permission === "always_ask"
                      ? compatibility.approval === "supported" ? app.t("HITL before use", "执行前 HITL") : app.t("HITL unsupported", "不支持 HITL")
                      : app.t("Always allow", "始终允许")}
                  </Pill>
                </div>;
              })}
            </div>
          )}
        </div>
      )}
      {resources.length === 0 ? (
        <span className="mut">{app.t("No Resource bindings are included.", "本次不包含资源绑定。")}</span>
      ) : (
        <div role="list" aria-label={app.t("Resources included in this publication", "本次发布包含的资源")} className="stack" style={{ gap: 8 }}>
          {resources.map((resource) => (
            <div
              role="listitem"
              key={resource.binding_id}
              className="row"
              style={{ alignItems: "baseline", gap: 8, flexWrap: "wrap" }}
            >
              <Pill tone="neutral">{resource.target.kind}</Pill>
              <strong className="mono">{resource.mount_path}</strong>
              <span className="mut mono" style={{ fontSize: 11 }}>{resource.target.id}</span>
              <Pill tone={resource.access === "read_write" ? "warn" : "ok"}>{resource.access}</Pill>
              {resource.instructions && <span className="mut">{resource.instructions}</span>}
            </div>
          ))}
        </div>
      )}
      {config && (
        <div className="publication-collaboration-summary">
          <div className="row" style={{ justifyContent: "space-between", alignItems: "baseline" }}>
            <strong>{app.t("Frozen collaboration roster", "冻结的协作成员")}</strong>
            <span className="mut mono">
              {app.t("depth", "层级")} {limits.max_depth}
              {" · "}{app.t("parallel", "并行")} {limits.max_parallel}
              {" · "}{app.t("total", "总次数")} {limits.max_total}
            </span>
          </div>
          {delegates.length === 0 ? (
            <p className="mut">{app.t("No auxiliary Agents are included.", "本次不包含辅助 Agent。")}</p>
          ) : (
            <div className="row" role="list" aria-label={app.t("Auxiliary Agents included in this publication", "本次发布包含的辅助 Agent")} style={{ flexWrap: "wrap" }}>
              {delegates.map((target, index) => (
                <Pill role="listitem" key={`${target.id}-${index}`} tone={target.recursiveSelf ? "warn" : "agent"}>
                  {target.recursiveSelf
                    ? app.t("built-in auxiliary · inherited", "内置辅助 Agent · 继承主 Agent")
                    : target.version !== undefined
                      ? `${target.id} · v${target.version}`
                      : app.t(`${target.id} · resolve at publish`, `${target.id} · 发布时解析`)}
                </Pill>
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
