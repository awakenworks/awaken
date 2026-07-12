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
import type { FileArtifact, Page, SessionResourceDto } from "../../lib/api/types";
import { useApp } from "../../lib/app-state";

const KIND_TONE: Record<string, "agent" | "info" | "ok" | "neutral"> = {
  memory_store: "agent",
  file: "info",
  github_repository: "ok",
};

export default function SessionFiles({ base, sid }: { base: string; sid: string }) {
  const app = useApp();
  const toast = useToast();

  const resources = useQuery({
    queryKey: ["session-resources", sid],
    queryFn: () => api.get<Page<SessionResourceDto>>(`${base}/resources`),
    retry: false,
  });
  const artifacts = useQuery({
    queryKey: ["session-artifacts", sid],
    // scope_id must be the raw session id (the backend keys artifacts by it).
    queryFn: () => api.get<{ data: FileArtifact[] }>(ws(`/v1/files?scope_id=${sid}`)),
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
      <div>
        <h2 style={{ fontSize: 12, textTransform: "uppercase", letterSpacing: ".06em", color: "var(--fg3)" }}>
          {app.t("Mounted resources", "挂载的资源")}
        </h2>
        {mounts.length === 0 ? (
          <span className="mut">{app.t("No resources mounted for this session.", "本会话未挂载资源。")}</span>
        ) : (
          <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
            {mounts.map((r) => (
              <div key={r.id} className="row" style={{ justifyContent: "space-between", gap: 8 }}>
                <span className="row" style={{ gap: 8 }}>
                  <Pill tone={KIND_TONE[r.type] ?? "neutral"}>{r.type}</Pill>
                  <code style={{ fontSize: 12 }}>{r.mount_path}</code>
                </span>
                <span className="mut mono" style={{ fontSize: 11 }}>
                  {r.memory_store_id ?? r.file_id ?? r.url ?? ""}
                </span>
              </div>
            ))}
          </div>
        )}
      </div>

      <div>
        <h2 style={{ fontSize: 12, textTransform: "uppercase", letterSpacing: ".06em", color: "var(--fg3)" }}>
          {app.t("Output artifacts", "输出产物")}
        </h2>
        {files.length === 0 ? (
          <EmptyState
            title={app.t("No artifacts yet.", "还没有产物。")}
            hint={app.t("Files the agent writes under its outputs mount appear here.", "agent 在 outputs 挂载里写的文件会出现在这里。")}
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
      </div>
    </Card>
  );
}
