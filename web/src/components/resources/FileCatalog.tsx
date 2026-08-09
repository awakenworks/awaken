import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useMemo, useRef, useState } from "react";
import { Link } from "react-router";
import { DataGrid, type Column } from "../ui/DataGrid";
import { Button, Pill, Segmented } from "../ui";
import { useToast } from "../ui/Toast";
import { api, ws } from "../../lib/api/client";
import type { FileArtifact, FileListResponse } from "../../lib/api/types";
import { useApp } from "../../lib/app-state";
import { useListState } from "../../lib/useListState";

function bytesLabel(value: number | undefined): string {
  if (value == null) return "—";
  if (value < 1024) return `${value} B`;
  if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KB`;
  return `${(value / (1024 * 1024)).toFixed(1)} MB`;
}

interface FileTreeRow {
  key: string;
  path: string;
  depth: number;
  directory: boolean;
  file?: FileArtifact;
}

export function fileTreeRows(files: readonly FileArtifact[]): FileTreeRow[] {
  const directories = new Set<string>();
  const paths = files.map((file) => ({ file, path: (file.logical_path || file.filename).replace(/^\/+/, "") }));
  for (const { path } of paths) {
    const parts = path.split("/").filter(Boolean);
    for (let index = 1; index < parts.length; index += 1) directories.add(parts.slice(0, index).join("/"));
  }
  return [
    ...Array.from(directories, (path) => ({ key: `dir:${path}`, path, depth: path.split("/").length - 1, directory: true })),
    ...paths.map(({ file, path }) => ({ key: file.id, path, depth: Math.max(0, path.split("/").length - 1), directory: false, file })),
  ].sort((left, right) => left.path.localeCompare(right.path));
}

export default function FileCatalog({ purpose }: { purpose: "input" | "artifact" }) {
  const app = useApp();
  const toast = useToast();
  const queryClient = useQueryClient();
  const picker = useRef<HTMLInputElement>(null);
  const [view, setView] = useState<"tree" | "list">("tree");
  const list = useListState("created_at");
  const files = useQuery({
    queryKey: ["files", app.workspaceId, purpose],
    queryFn: () => api.get<FileListResponse>(ws(`/v1/files?purpose=${purpose}&limit=1000`)),
    refetchInterval: purpose === "artifact" ? 15_000 : false,
  });
  const upload = useMutation({
    mutationFn: (file: File) => api.upload<FileArtifact>(ws("/v1/files"), file),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ["files", app.workspaceId, "input"] });
      toast.ok(app.t("File uploaded.", "文件已上传。"));
    },
    onError: (error) => toast.err(error instanceof Error ? error.message : String(error)),
  });
  const download = (file: FileArtifact) =>
    api.download(ws(`/v1/files/${file.id}/content`), file.filename).catch((error) =>
      toast.err(error instanceof Error ? error.message : app.t("Download failed.", "下载失败。")),
    );

  const columns: Column<FileArtifact>[] = [
    {
      key: "filename",
      header: app.t(purpose === "artifact" ? "Artifact" : "File", purpose === "artifact" ? "产物" : "文件"),
      sortValue: (file) => file.logical_path || file.filename,
      cell: (file) => (
        <span style={{ display: "flex", flexDirection: "column", gap: 3 }}>
          <code>{file.logical_path || file.filename}</code>
          {file.logical_path && file.logical_path !== file.filename && (
            <span className="mut">{file.filename}</span>
          )}
        </span>
      ),
    },
    ...(purpose === "artifact"
      ? [{
          key: "session",
          header: app.t("Session", "会话"),
          sortValue: (file: FileArtifact) => file.session_id ?? "",
          cell: (file: FileArtifact) => file.session_id ? (
            <Link to={`/w/${app.workspaceId}/sessions/${file.session_id}`}>
              <code>{file.session_id}</code>
            </Link>
          ) : <span className="mut">—</span>,
        } satisfies Column<FileArtifact>]
      : [{
          key: "availability",
          header: app.t("Use", "用途"),
          cell: () => <Pill tone="info">{app.t("Agent input", "Agent 输入")}</Pill>,
        } satisfies Column<FileArtifact>]),
    {
      key: "type",
      header: app.t("Type", "类型"),
      sortValue: (file) => file.mime_type ?? "",
      cell: (file) => <span className="mut">{file.mime_type || "—"}</span>,
    },
    {
      key: "size",
      header: app.t("Size", "大小"),
      sortValue: (file) => file.size_bytes ?? 0,
      cell: (file) => <span className="mut">{bytesLabel(file.size_bytes)}</span>,
    },
    {
      key: "created_at",
      header: app.t("Created", "创建时间"),
      sortValue: (file) => file.created_at ?? "",
      cell: (file) => <span className="mut">{file.created_at || "—"}</span>,
    },
    ...(purpose === "artifact"
      ? [{
          key: "actions",
          header: "",
          cell: (file: FileArtifact) => (
            <Button
              variant="ghost"
              disabled={file.downloadable === false}
              onClick={() => void download(file)}
            >
              ↓ {app.t("Download", "下载")}
            </Button>
          ),
        } satisfies Column<FileArtifact>]
      : []),
  ];

  const rows = files.data?.data ?? [];
  const tree = useMemo(() => fileTreeRows(rows), [rows]);
  return (
    <>
      <div className="row" style={{ justifyContent: "space-between", alignItems: "flex-start" }}>
        <p className="mut" style={{ margin: 0, maxWidth: 760 }}>
          {purpose === "artifact"
            ? app.t(
                "These read-only files were produced by Sessions. Open the linked Session to understand how an artifact was created.",
                "这些只读文件由会话生成。需要了解产物的生成过程时，请打开关联会话。",
              )
            : app.t(
                "Upload inputs that can be reused by Agents in this Workspace. Files become available during runs only after you bind them to an Agent.",
                "上传当前工作区内可复用的 Agent 输入。文件只有绑定到 Agent 后，才会在运行中可用。",
              )}
        </p>
        <span className="row">
          <Segmented value={view} onChange={setView} options={[{ value: "tree", label: app.t("Folders", "目录") }, { value: "list", label: app.t("List", "列表") }]} />
          {purpose === "input" && <>
            <input
              ref={picker}
              type="file"
              hidden
              aria-label={app.t("Choose file", "选择文件")}
              onChange={(event) => {
                const file = event.target.files?.[0];
                if (file) upload.mutate(file);
                event.target.value = "";
              }}
            />
            <Button
              variant="primary"
              disabled={upload.isPending}
              onClick={() => picker.current?.click()}
            >
              + {upload.isPending ? app.t("Uploading…", "上传中…") : app.t("Upload file", "上传文件")}
            </Button>
          </>}
        </span>
      </div>
      {files.error instanceof Error && <div className="err">{files.error.message}</div>}
      {view === "list" ? <DataGrid
        rows={rows}
        columns={columns}
        rowKey={(file) => file.id}
        state={list}
        loading={files.isLoading}
        filter={(file, query) => [
          file.id,
          file.filename,
          file.logical_path,
          file.session_id,
          file.mime_type,
        ].some((value) => value?.toLowerCase().includes(query.toLowerCase()))}
        searchPlaceholder={app.t(
          purpose === "artifact" ? "Filter artifacts…" : "Filter files…",
          purpose === "artifact" ? "过滤产物…" : "过滤文件…",
        )}
        emptyTitle={app.t(
          purpose === "artifact" ? "No artifacts yet." : "No input files yet.",
          purpose === "artifact" ? "还没有产物。" : "还没有输入文件。",
        )}
        emptyHint={app.t(
          purpose === "artifact"
            ? "Files written by Agents under their output mount appear here."
            : "Upload a file, then bind it to an Agent under Memory & resources.",
          purpose === "artifact"
            ? "Agent 写入输出挂载目录的文件会显示在这里。"
            : "上传文件后，可在 Agent 的 Memory 与资源中绑定。",
        )}
      /> : <div className="card" style={{ padding: 0, overflow: "hidden" }}>
        <div className="mut" style={{ padding: "10px 14px", borderBottom: "1px solid var(--line)" }}>{app.t("Names containing / are grouped into folders for easier browsing. Moving between folder and list views does not change the files.", "名称中的 / 会显示为目录，便于浏览。切换目录或列表视图不会修改文件。")}</div>
        <table className="table"><thead><tr><th>{app.t("Folder / file", "目录 / 文件")}</th><th>{app.t("Type", "类型")}</th><th>{app.t("Size", "大小")}</th>{purpose === "artifact" && <th />}</tr></thead><tbody>
          {tree.map((row) => row.directory ? <tr key={row.key}><td colSpan={purpose === "artifact" ? 4 : 3} className="mono mut" style={{ paddingLeft: 14 + row.depth * 18 }}>▾ {row.path.split("/").at(-1)}/</td></tr> : row.file && <tr key={row.key}><td style={{ paddingLeft: 14 + row.depth * 18 }}><code>└ {row.path.split("/").at(-1)}</code><div className="mut mono">{row.file.id}</div></td><td className="mut">{row.file.mime_type || "—"}</td><td className="mut">{bytesLabel(row.file.size_bytes)}</td>{purpose === "artifact" && <td style={{ textAlign: "right" }}><Button variant="ghost" disabled={row.file.downloadable === false} onClick={() => void download(row.file!)}>↓ {app.t("Download", "下载")}</Button></td>}</tr>)}
          {!files.isLoading && tree.length === 0 && <tr><td colSpan={purpose === "artifact" ? 4 : 3} className="mut">{app.t(purpose === "artifact" ? "No artifacts yet." : "No input files yet.", purpose === "artifact" ? "还没有产物。" : "还没有输入文件。")}</td></tr>}
        </tbody></table>
      </div>}
    </>
  );
}
