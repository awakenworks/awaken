import { useMutation, useQuery } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { useParams } from "react-router";
import { api } from "../lib/api/client";
import type { Project } from "../lib/api/types";
import { useApp } from "../lib/app-state";

const WORKSPACE = "wrkspc_default";

export default function ProjectSettingsSurface() {
  const app = useApp();
  const { pid = "" } = useParams();
  const project = useQuery({
    queryKey: ["project", pid],
    queryFn: () => api.get<Project>(`/v1/config/projects/${pid}`),
    retry: false,
  });
  const [name, setName] = useState("");
  useEffect(() => {
    if (project.data) setName(project.data.display_name);
  }, [project.data]);
  const save = useMutation({
    mutationFn: () =>
      api.put<Project>(`/v1/config/projects/${pid}`, {
        id: pid,
        workspace_id: project.data?.workspace_id ?? WORKSPACE,
        display_name: name,
        version: (project.data?.version ?? 0) + 1,
      }),
    onSuccess: () => void project.refetch(),
  });
  return (
    <>
      <div className="banner warn">
        <span>⚑</span>
        <span>
          {app.t("Scoped to project", "作用域:项目")} · <code>{pid}</code> —{" "}
          {app.t("these settings affect only this project.", "仅影响本项目。")}
        </span>
      </div>
      <div className="card">
        <h2>General</h2>
        <div className="row">
          <span className="field">
            <label>Project id</label>
            <input className="input mono" value={pid} disabled />
          </span>
          <span className="field" style={{ flex: 1 }}>
            <label>{app.t("Display name", "显示名")}</label>
            <input className="input" value={name} onChange={(e) => setName(e.target.value)} />
          </span>
          <button className="btn primary" style={{ alignSelf: "flex-end" }} onClick={() => save.mutate()}>
            {app.t("Save", "保存")}
          </button>
        </div>
        {save.error instanceof Error && <div className="err">{save.error.message}</div>}
      </div>
      <div className="card">
        <h2>Ingress</h2>
        <p className="hint">
          {app.t(
            "The project id doubles as the ingress address segment. Addressing only — authority still flows from the API key (ADR-0042).",
            "项目 id 同时是 ingress 地址段。仅寻址——授权仍来自 API key(ADR-0042)。",
          )}
        </p>
        <div className="row">
          <code>{location.origin}/projects/{pid}</code>
          <button
            className="btn ghost"
            onClick={() => navigator.clipboard.writeText(`${location.origin}/projects/${pid}`)}
          >
            copy
          </button>
        </div>
      </div>
    </>
  );
}
