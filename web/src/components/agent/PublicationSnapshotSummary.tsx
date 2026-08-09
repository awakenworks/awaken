import type { InputBinding } from "../../lib/api/types";
import { useApp } from "../../lib/app-state";
import { Pill } from "../ui";

export default function PublicationSnapshotSummary({
  sourceRevision,
  resourceRevision,
  resources,
}: {
  sourceRevision?: number;
  resourceRevision: number;
  resources: InputBinding[];
}) {
  const app = useApp();

  return (
    <div style={{ marginTop: 16, paddingTop: 14, borderTop: "1px solid var(--line)" }}>
      <div className="row" style={{ justifyContent: "space-between", alignItems: "baseline" }}>
        <strong>{app.t("Publication snapshot", "发布快照")}</strong>
        <span className="mut mono" style={{ fontSize: 11 }}>
          {app.t("Agent revision", "Agent 版本")} {sourceRevision ?? "—"}
          {" · "}
          {app.t("Resource revision", "资源版本")} {resourceRevision}
        </span>
      </div>
      <p className="mut" style={{ margin: "6px 0 10px" }}>
        {app.t(
          "The Agent configuration and these Resource bindings will be frozen together for new runs.",
          "Agent 配置与以下资源绑定会一起冻结，供新的运行使用。",
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
    </div>
  );
}
