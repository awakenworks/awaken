import type { AgentConfig, InputBinding } from "../../lib/api/types";
import { DEFAULT_DELEGATION_LIMITS, rosterOf } from "../../lib/agent-collaboration";
import { useApp } from "../../lib/app-state";
import { Pill } from "../ui";

export default function PublicationSnapshotSummary({
  sourceRevision,
  resourceRevision,
  resources,
  config,
}: {
  sourceRevision?: number;
  resourceRevision: number;
  resources: InputBinding[];
  config?: Pick<AgentConfig, "id" | "multiagent" | "delegation_limits" | "metadata">;
}) {
  const app = useApp();
  const delegates = config ? rosterOf(config) : [];
  const limits = config?.delegation_limits ?? DEFAULT_DELEGATION_LIMITS;

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
