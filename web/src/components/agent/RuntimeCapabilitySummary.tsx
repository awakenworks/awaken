import type { AgentConfig, RuntimeCap } from "../../lib/api/types";
import { isAcpModelSelection } from "../../lib/agent-model-selection";
import {
  runtimeCapabilitySupport,
  type RuntimeSupport as Support,
} from "../../lib/agent-runtime-capabilities";
import { useApp } from "../../lib/app-state";
import { Pill } from "../ui";

interface SupportRow {
  capability: string;
  support: Support;
  detail: string;
}

export default function RuntimeCapabilitySummary({
  model,
  runtime,
}: {
  model: AgentConfig["model"];
  runtime?: RuntimeCap;
}) {
  const app = useApp();
  const acp = isAcpModelSelection(model);
  const support = runtimeCapabilitySupport(acp, runtime?.features);
  const rows: SupportRow[] = [
    {
      capability: app.t("Environment and durable Session", "Environment 与持久 Session"),
      support: support.environment_session,
      detail: app.t(
        "Environment isolation, files, event history, recovery, Trace, approvals, and Deployments remain owned by Awaken.",
        "Environment 隔离、文件、事件历史、恢复、Trace、审批和 Deployment 仍由 Awaken 负责。",
      ),
    },
    {
      capability: app.t("Skills, Memory, and compaction", "Skills、Memory 与压缩"),
      support: support.context,
      detail: acp
        ? app.t(
            "Awaken projects prepared context into the external harness and keeps the durable source outside it.",
            "Awaken 将准备好的上下文投影给外部 Harness，持久数据源仍保留在 Harness 之外。",
          )
        : app.t(
            "The Native runtime applies these capabilities directly inside the Session lifecycle.",
            "Native Runtime 在 Session 生命周期内直接应用这些能力。",
          ),
    },
    {
      capability: app.t("Tools, ToolSets, and MCP", "工具、ToolSet 与 MCP"),
      support: support.tools_mcp,
      detail: acp
        ? app.t(
            "Awaken policies still apply. Host tool export and authenticated MCP also require support from the selected Harness and Environment.",
            "Awaken 权限策略仍然生效；Host 工具导出和带凭据 MCP 还需要所选 Harness 与 Environment 支持。",
          )
        : app.t(
            "Native tools, ToolSet policies, Custom Tools, and MCP use the Runtime's direct execution path.",
            "Native 工具、ToolSet 策略、Custom Tool 与 MCP 使用 Runtime 的直接执行路径。",
          ),
    },
    {
      capability: app.t("State machine and tool concurrency", "状态机与工具并发"),
      support: support.state_concurrency,
      detail: acp
        ? app.t(
            "State-machine configuration is unavailable because the external Harness owns the model/tool loop. Awaken still enforces concurrency on tools it executes.",
            "状态机配置不可用，因为外部 Harness 掌握模型与工具循环；Awaken 仍对自身执行的工具实施并发控制。",
          )
        : app.t(
            "The Native runtime applies the configured transitions, guards, and concurrency limits directly.",
            "Native Runtime 直接应用已配置的转换、守卫与并发限制。",
          ),
    },
    {
      capability: app.t("Background tool execution", "后台工具执行"),
      support: support.background_tools,
      detail: acp
        ? app.t(
            "This currently requires the Native process so it can retain an identity-bound prepared executor.",
            "该能力目前需要 Native 进程保留与身份绑定的已准备执行器。",
          )
        : app.t(
            "Configure an allowlist of existing tools. Their ToolSet permission, concurrency, recovery, and resource policies still apply.",
            "配置已有工具允许列表；其 ToolSet 权限、并发、恢复与资源策略仍然生效。",
          ),
    },
  ];

  const supportLabel = (support: Support) => support === "supported"
    ? app.t("Supported", "支持")
    : support === "conditional"
      ? app.t("Conditional", "有条件支持")
      : app.t("Unavailable", "不可用");
  const supportTone = (support: Support) => support === "supported"
    ? "ok" as const
    : support === "conditional"
      ? "warn" as const
      : "neutral" as const;
  const negotiated = runtime?.local?.negotiated;

  return (
    <details className="runtime-capability-summary">
      <summary>
        <span>
          <strong>{app.t("Capability support", "能力支持")}</strong>
          <span className="mut"> · {runtime?.label ?? app.t("Native Awaken", "Native Awaken")}</span>
        </span>
        <span className="mut">{app.t("View matrix", "查看矩阵")}</span>
      </summary>
      <div className="runtime-capability-grid">
        {rows.map((row) => (
          <div className="runtime-capability-row" key={row.capability}>
            <div>
              <strong>{row.capability}</strong>
              <p>{row.detail}</p>
            </div>
            <Pill tone={supportTone(row.support)}>{supportLabel(row.support)}</Pill>
          </div>
        ))}
      </div>
      {acp && runtime?.local?.remediation && (
        <div className="banner warn">
          <span>!</span>
          <span>{runtime.local.remediation}</span>
        </div>
      )}
      {acp && support.tools_mcp !== "supported" && (
        <div className="banner warn" role="alert">
          <span>!</span>
          <span>{app.t(
            negotiated
              ? "This Harness did not negotiate a compatible MCP transport. Awaken read, write, Memory, Skill, and host Web tools will not be available."
              : "Tool transport has not been verified yet. Refresh the Harness probe before publishing an Agent that uses Awaken tools.",
            negotiated
              ? "该 Harness 未协商到兼容的 MCP 传输；Awaken read、write、Memory、Skill 和 Host Web 工具将不可用。"
              : "工具传输尚未验证。发布使用 Awaken 工具的 Agent 前，请先刷新 Harness 能力探测。",
          )}</span>
        </div>
      )}
    </details>
  );
}
