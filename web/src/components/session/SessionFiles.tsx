// The session's associated resources + output artifacts (Anthropic Managed Agents
// parity): the memory stores / files / repos MOUNTED for this session
// (`GET /v1/sessions/:id/resources`), and the files the agent PRODUCED under `outputs/`
// (`GET /v1/files?scope_id=<session>`, downloadable via the content endpoint). Both are
// pure projections of backend truth; polling the artifacts list also flushes the
// session's memory write-back (a backend side effect of the reverse channel).

import { useQuery } from "@tanstack/react-query";
import { Button, Card, EmptyState, Pill } from "../ui";
import { useToast } from "../ui/Toast";
import { api, ws } from "../../lib/api/client";
import type {
  FileArtifact,
  ListSessionResourcesResponse,
  SessionResource,
} from "../../lib/api/types";
import { useApp } from "../../lib/app-state";

const KIND_TONE: Record<SessionResource["type"], "agent" | "info" | "ok"> = {
  memory_store: "agent",
  file: "info",
  github_repository: "ok",
};

function resourceView(resource: SessionResource): {
  key: string;
  identity: string;
  mountPath: string | null;
} {
  switch (resource.type) {
    case "file":
      return {
        key: `file:${resource.id}`,
        identity: resource.file_id,
        mountPath: resource.mount_path,
      };
    case "github_repository":
      return {
        key: `github_repository:${resource.id}`,
        identity: resource.url,
        mountPath: resource.mount_path,
      };
    case "memory_store":
      return {
        key: `memory_store:${resource.memory_store_id}`,
        identity: resource.memory_store_id,
        mountPath: resource.mount_path ?? null,
      };
  }
}

export default function SessionFiles({
  base,
  sid,
  view,
}: {
  base: string;
  sid: string;
  view: "inputs" | "artifacts";
}) {
  const app = useApp();
  const toast = useToast();

  const resources = useQuery({
    queryKey: ["session-resources", sid],
    queryFn: () => api.get<ListSessionResourcesResponse>(`${base}/resources`),
    enabled: view === "inputs",
    retry: false,
  });
  const artifacts = useQuery({
    queryKey: ["session-artifacts", sid],
    // scope_id must be the raw session id (the backend keys artifacts by it).
    queryFn: () => api.get<{ data: FileArtifact[] }>(ws(`/v1/files?scope_id=${sid}`)),
    enabled: view === "artifacts",
    retry: false,
    refetchInterval: 15_000,
  });

  const mounts = resources.data?.data ?? [];
  const files = artifacts.data?.data ?? [];
  const download = (f: FileArtifact) =>
    api.download(ws(`/v1/files/${f.id}/content`), f.filename).catch((e) =>
      toast.err(e instanceof Error ? e.message : "download failed"),
    );

  return (
    <Card style={{ marginTop: 10, display: "flex", flexDirection: "column", gap: 16 }}>
      {view === "inputs" && <div>
        <h2 style={{ fontSize: 12, textTransform: "uppercase", letterSpacing: ".06em", color: "var(--fg3)" }}>
          {app.t("Session inputs", "Session 输入")}
        </h2>
        <p className="hint">{app.t(
          "Files can be added while this Session runs. Its selected Memory and repository stay the same for the life of the Session.",
          "Session 运行期间可以继续添加文件；创建时选择的 Memory 和代码仓会在本次 Session 中保持不变。",
        )}</p>
        {mounts.length === 0 ? (
          <span className="mut">{app.t("No resources mounted for this session.", "本会话未挂载资源。")}</span>
        ) : (
          <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
            {mounts.map((resource) => {
              const item = resourceView(resource);
              return (
                <div key={item.key} className="row" style={{ justifyContent: "space-between", gap: 8 }}>
                  <span className="row" style={{ gap: 8 }}>
                    <Pill tone={KIND_TONE[resource.type]}>{resource.type}</Pill>
                    <code style={{ fontSize: 12 }}>
                      {item.mountPath ?? app.t("not mounted", "未挂载")}
                    </code>
                  </span>
                  <span className="mut mono" style={{ fontSize: 11 }}>{item.identity}</span>
                </div>
              );
            })}
          </div>
        )}
      </div>}

      {view === "artifacts" && <div>
        <h2 style={{ fontSize: 12, textTransform: "uppercase", letterSpacing: ".06em", color: "var(--fg3)" }}>
          {app.t("Session artifacts", "Session 产物")}
        </h2>
        <p className="hint">{app.t(
          "Files produced by this Session appear here for review and download.",
          "当前 Session 生成的文件会显示在这里，供你查看和下载。",
        )}</p>
        {files.length === 0 ? (
          <EmptyState
            title={app.t("No artifacts yet.", "还没有产物。")}
            hint={app.t("Files created by the Agent will appear here after the run saves them.", "Agent 创建并保存文件后，它们会出现在这里。")}
          />
        ) : (
          <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
            {files.map((f) => (
              <div key={f.id} className="row" style={{ justifyContent: "space-between", gap: 8 }}>
                <code style={{ fontSize: 12 }}>{f.filename}</code>
                <Button variant="ghost" style={{ height: 24 }} disabled={f.downloadable === false} onClick={() => download(f)}>
                  ↓ {app.t("download", "下载")}
                </Button>
              </div>
            ))}
          </div>
        )}
      </div>}
    </Card>
  );
}
