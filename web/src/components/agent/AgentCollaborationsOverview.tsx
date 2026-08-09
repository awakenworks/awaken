import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { useNavigate } from "react-router";
import type { Agent, AgentConfigItem, Page } from "../../lib/api/types";
import { api, ws } from "../../lib/api/client";
import { projectCollaborations, rosterOf } from "../../lib/agent-collaboration";
import { useApp } from "../../lib/app-state";
import { Button, Card, EmptyState, Pill, TextField } from "../ui";

export default function AgentCollaborationsOverview({ agents }: { agents: AgentConfigItem[] }) {
  const app = useApp();
  const nav = useNavigate();
  const [filter, setFilter] = useState("");
  const projection = useMemo(() => projectCollaborations(agents), [agents]);
  const agentById = useMemo(() => new Map(agents.map((agent) => [agent.id, agent])), [agents]);
  const releases = useQuery({
    queryKey: ["managed-agents", app.workspaceId],
    queryFn: () => api.get<Page<Agent>>(ws("/v1/agents?limit=100")),
  });
  const releaseById = useMemo(
    () => new Map((releases.data?.data ?? []).map((agent) => [agent.id, agent])),
    [releases.data?.data],
  );
  const normalizedFilter = filter.trim().toLowerCase();
  const coordinators = projection.coordinators.filter((coordinator) => {
    if (!normalizedFilter) return true;
    return [coordinator.id, coordinator.name, ...rosterOf(coordinator).map((target) => target.id)]
      .some((value) => value?.toLowerCase().includes(normalizedFilter));
  });

  return (
    <div className="collaboration-overview">
      <div className="collaboration-summary" aria-label={app.t("Collaboration summary", "协作关系摘要")}>
        <Card><strong>{projection.coordinators.length}</strong><span>{app.t("Coordinators", "协调 Agent")}</span></Card>
        <Card><strong>{projection.referencedAgentIds.size}</strong><span>{app.t("Attached specialists", "专属辅助 Agent")}</span></Card>
        <Card><strong>{projection.recursiveCoordinatorIds.size}</strong><span>{app.t("Built-in auxiliaries", "内置辅助 Agent")}</span></Card>
        <Card className={projection.brokenReferences.length ? "collaboration-summary-danger" : ""}>
          <strong>{projection.brokenReferences.length}</strong><span>{app.t("Broken references", "失效引用")}</span>
        </Card>
      </div>
      <Card className="collaboration-guidance">
        <div>
          <h2>{app.t("One relationship owner", "单一关系所有者")}</h2>
          <p className="hint">{app.t(
            "This page is a workspace projection. Edit a roster from its coordinator so publication review remains the only way relationships become active.",
            "此页面是工作区关系投影。请从协调 Agent 编辑成员清单，确保协作关系只通过发布审查生效。",
          )}</p>
        </div>
        <TextField
          aria-label={app.t("Filter collaborations", "筛选协作关系")}
          placeholder={app.t("Filter coordinators or auxiliary Agents…", "筛选协调或辅助 Agent…")}
          value={filter}
          onChange={(event) => setFilter(event.target.value)}
        />
      </Card>

      {coordinators.length === 0 ? (
        <Card>
          <EmptyState
            title={normalizedFilter
              ? app.t("No matching collaboration.", "没有匹配的协作关系。")
              : app.t("No Agent collaborations yet.", "还没有 Agent 协作关系。")}
            hint={app.t(
            "Every primary Agent starts with its built-in auxiliary. Open Advanced → Orchestration to add parent-owned specialists.",
            "每个主 Agent 默认都有内置辅助 Agent；可在“高级 → 编排”中添加归属于它的专属辅助 Agent。",
            )}
            action={!normalizedFilter ? <Button variant="primary" onClick={() => nav(`/w/${app.workspaceId}/agents/new`)}>+ {app.t("New Agent", "新建 Agent")}</Button> : undefined}
          />
        </Card>
      ) : (
        <div className="coordinator-list">
          {coordinators.map((coordinator) => {
            const targets = rosterOf(coordinator);
            return (
              <Card className="coordinator-card" key={coordinator.id}>
                <div className="coordinator-head">
                  <div>
                    <div className="row" style={{ gap: 8 }}>
                      <h2>{coordinator.name || coordinator.id}</h2>
                      <Pill tone={coordinator.published ? "ok" : "warn"}>
                        {coordinator.published ? app.t("published", "已发布") : app.t("draft", "草稿")}
                      </Pill>
                    </div>
                    <span className="mono mut">{coordinator.id}</span>
                  </div>
                  <Button onClick={() => nav(`/w/${app.workspaceId}/agents/${coordinator.id}?stage=advanced&section=orchestration`)}>
                    {app.t("Edit roster", "编辑成员")} →
                  </Button>
                </div>
                <div className="relationship-flow" role="list" aria-label={`${coordinator.id} ${app.t("auxiliary Agents", "辅助 Agent")}`}>
                  <span className="relationship-origin">{coordinator.name || coordinator.id}</span>
                  <span aria-hidden="true">→</span>
                  <div className="relationship-targets">
                    {targets.map((target, index) => {
                      const item = agentById.get(target.id);
                      const release = releaseById.get(target.id);
                      return (
                        <div role="listitem" className="relationship-target" key={`${target.id}-${index}`}>
                          <span>
                            <strong>{target.recursiveSelf ? app.t("Built-in auxiliary", "内置辅助 Agent") : item?.name || release?.name || target.id}</strong>
                            <small className="mono">{target.recursiveSelf
                              ? app.t(`inherits ${coordinator.id}`, `继承 ${coordinator.id}`)
                              : target.id}</small>
                          </span>
                          {target.recursiveSelf ? (
                            <Pill tone="agent">{app.t("default", "默认")}</Pill>
                          ) : !item ? (
                            <Pill tone="danger">{app.t("missing", "缺失")}</Pill>
                          ) : !item.published ? (
                            <Pill tone="warn">{app.t("draft only", "仅草稿")}</Pill>
                          ) : target.version !== undefined ? (
                            <Pill tone="agent">{app.t(`pinned v${target.version}`, `固定 v${target.version}`)}</Pill>
                          ) : (
                            <Pill tone="neutral">{release?.version
                              ? app.t(`resolve at publish · current v${release.version}`, `发布时解析 · 当前 v${release.version}`)
                              : app.t("resolve at publish", "发布时解析")}</Pill>
                          )}
                        </div>
                      );
                    })}
                  </div>
                </div>
              </Card>
            );
          })}
        </div>
      )}

      {projection.unusedAgents.length > 0 && (
        <Card className="unused-agents">
          <h2>{app.t("Attached specialists not in the active roster", "尚未加入当前成员清单的专属辅助 Agent")}</h2>
          <p className="hint">{app.t(
            "They remain owned by their primary Agent, but are not included in its next publication until selected in Orchestration.",
            "它们仍归属于对应主 Agent，但只有在“编排”中选中后才会进入主 Agent 的下一次发布。",
          )}</p>
          <div className="row" style={{ flexWrap: "wrap" }}>
            {projection.unusedAgents.map((agent) => <Pill key={agent.id}>{agent.name || agent.id}</Pill>)}
          </div>
        </Card>
      )}
    </div>
  );
}
