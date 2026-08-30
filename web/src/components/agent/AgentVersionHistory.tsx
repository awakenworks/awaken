import { useQuery } from "@tanstack/react-query";
import { api, isAbsent, ws } from "../../lib/api/client";
import type { Agent, CursorPage } from "../../lib/api/types";
import { useApp } from "../../lib/app-state";
import { dateTimeLabel } from "../../lib/presentation";
import { Pill, TechnicalId } from "../ui";

export function orderedAgentVersions(values: Agent[] | undefined): Agent[] {
  return [...(values ?? [])].sort((left, right) => right.version - left.version);
}

export function agentVersionModel(agent: Agent): string {
  return typeof agent.model === "string" ? agent.model : agent.model.id;
}

export default function AgentVersionHistory({ agentId, enabled }: { agentId: string; enabled: boolean }) {
  const app = useApp();
  const versions = useQuery({
    queryKey: ["agent-versions", app.workspaceId, agentId],
    queryFn: () => api.get<CursorPage<Agent>>(ws(`/v1/agents/${encodeURIComponent(agentId)}/versions?limit=500`)),
    enabled: enabled && agentId.trim().length > 0,
    retry: (failures, error) => !isAbsent(error) && failures < 2,
  });
  const rows = orderedAgentVersions(versions.data?.data);

  return (
    <section className="agent-version-history" aria-labelledby="agent-version-history-title">
      <div className="row" style={{ justifyContent: "space-between", alignItems: "flex-start" }}>
        <div>
          <h3 id="agent-version-history-title">{app.t("Published version history", "已发布版本历史")}</h3>
          <p className="hint">{app.t(
            "Only immutable publications appear here. Saving a draft never changes this list or an existing Session.",
            "这里只显示不可变的已发布版本。保存草稿不会改变此列表或已有 Session。",
          )}</p>
        </div>
        {rows.length > 0 && <Pill tone="neutral">{rows.length} {app.t("versions", "个版本")}</Pill>}
      </div>
      {versions.isLoading ? (
        <span className="mut">…</span>
      ) : versions.error && !isAbsent(versions.error) ? (
        <div className="err">{versions.error.message}</div>
      ) : rows.length === 0 ? (
        <p className="mut">{app.t("No published versions yet. Review the draft diff, then publish the first immutable version.", "还没有已发布版本。请检查草稿差异，再发布首个不可变版本。")}</p>
      ) : (
        <div role="list" aria-label={app.t("Published Agent versions", "已发布 Agent 版本")}>
          {rows.map((version, index) => (
            <div role="listitem" key={version.version} className="agent-version-row">
              <span>
                <strong>v{version.version}</strong>
                {index === 0 && <Pill tone="ok">{app.t("latest published", "最新已发布")}</Pill>}
              </span>
              <span className="mut">{agentVersionModel(version) || "—"}</span>
              <span className="mut">
                {version.tools.length} {app.t("tools", "工具")} · {version.mcp_servers.length} MCP · {version.skills.length} Skills
              </span>
              <span className="mut">{dateTimeLabel(version.updated_at, app.locale)}</span>
              <TechnicalId value={`${version.id}@${version.version}`} />
            </div>
          ))}
        </div>
      )}
      {versions.data?.next_page && (
        <p className="hint">{app.t(
          "More than 500 versions exist. Use the Managed Agents API cursor to inspect earlier history.",
          "版本超过 500 个；请使用 Managed Agents API 游标查看更早历史。",
        )}</p>
      )}
    </section>
  );
}
