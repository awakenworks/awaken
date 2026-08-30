// Workspace · Skills: one immutable, versioned bundle catalog. Local directory,
// ZIP import, and browser edits all publish through the same multipart Skills API;
// the browser owns only an ephemeral draft and never becomes a second content store.

import { useEffect, useMemo, useRef, useState, type InputHTMLAttributes } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, ws, type MultipartFile } from "../lib/api/client";
import type { Page, Skill, SkillVersion } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { Button, Card, EmptyState, Modal, Pill, Segmented, SkeletonRows, TechnicalId, TextAreaField, TextField, useConfirm, useToast } from "../components/ui";
import { identifierLabel } from "../lib/presentation";

export interface DraftFile {
  path: string;
  bytes: Uint8Array;
  executable: boolean;
}

const encoder = new TextEncoder();
const decoder = new TextDecoder();
const directoryInputAttributes = {
  webkitdirectory: "",
  directory: "",
} as InputHTMLAttributes<HTMLInputElement>;

function sameBytes(left: Uint8Array, right: Uint8Array): boolean {
  return left.length === right.length && left.every((value, index) => value === right[index]);
}

function textFile(file: DraftFile): boolean {
  if (file.bytes.includes(0)) return false;
  const extension = file.path.split(".").pop()?.toLowerCase();
  if (["md", "txt", "json", "yaml", "yml", "toml", "sh", "py", "js", "ts", "tsx", "jsx", "rs", "go", "xml", "csv"].includes(extension ?? "")) return true;
  try {
    new TextDecoder("utf-8", { fatal: true }).decode(file.bytes);
    return true;
  } catch {
    return false;
  }
}

function multipart(files: readonly DraftFile[]): MultipartFile[] {
  return files.map((file) => ({ path: file.path, blob: new Blob([file.bytes as BlobPart]) }));
}

export function skillNeedsSandbox(paths: readonly string[], skillMd: string): boolean {
  const supportFiles = paths.filter((path) => !path.endsWith("SKILL.md"));
  if (supportFiles.length > 0) return true;
  const frontmatter = skillMd.match(/^---\s*\n([\s\S]*?)\n---(?:\s*\n|$)/)?.[1] ?? "";
  return /^environment\s*:\s*(filesystem|hand)\s*$/im.test(frontmatter);
}

function tarText(bytes: Uint8Array, start: number, length: number): string {
  const end = bytes.indexOf(0, start);
  return decoder.decode(bytes.subarray(start, end >= start && end < start + length ? end : start + length)).trim();
}

/** Inspect the official beta Skill content archive without depending on
 * non-standard fields in the version metadata response. */
export function skillFilesFromTar(archive: Uint8Array): DraftFile[] {
  const files: DraftFile[] = [];
  for (let offset = 0; offset + 512 <= archive.length;) {
    const header = archive.subarray(offset, offset + 512);
    if (header.every((byte) => byte === 0)) break;
    const name = tarText(header, 0, 100);
    const prefix = tarText(header, 345, 155);
    const rawSize = tarText(header, 124, 12);
    const size = Number.parseInt(rawSize || "0", 8);
    const mode = Number.parseInt(tarText(header, 100, 8) || "0", 8);
    if (!Number.isSafeInteger(size) || size < 0 || offset + 512 + size > archive.length) {
      throw new Error("Invalid Skill content archive");
    }
    const fullPath = prefix ? `${prefix}/${name}` : name;
    const path = fullPath.split("/").slice(1).join("/") || fullPath;
    const kind = header[156];
    if (kind === 0 || kind === 48) {
      files.push({
        path,
        bytes: archive.slice(offset + 512, offset + 512 + size),
        executable: Number.isSafeInteger(mode) && (mode & 0o111) !== 0,
      });
    }
    offset += 512 + Math.ceil(size / 512) * 512;
  }
  if (!files.some((file) => file.path.endsWith("SKILL.md"))) {
    throw new Error("Skill content archive has no SKILL.md");
  }
  return files;
}

export function skillRuntimeFromTar(archive: Uint8Array): boolean {
  const files = skillFilesFromTar(archive);
  const skillMd = files.find((file) => file.path.endsWith("SKILL.md"));
  if (!skillMd) throw new Error("Skill content archive has no SKILL.md");
  return skillNeedsSandbox(files.map((file) => file.path), decoder.decode(skillMd.bytes));
}

function CreateSkillModal({ onClose, onCreated }: { onClose: () => void; onCreated: () => void }) {
  const app = useApp();
  const [id, setId] = useState("");
  const [title, setTitle] = useState("");
  const [description, setDescription] = useState("");
  const [environment, setEnvironment] = useState<"instruction-only" | "filesystem">("instruction-only");
  const [instructions, setInstructions] = useState("");
  const [error, setError] = useState("");
  const create = useMutation({
    mutationFn: () => {
      const skillId = id.trim();
      if (!skillId) throw new Error(app.t("Skill id is required.", "必须填写 Skill id。"));
      if (!instructions.trim()) throw new Error(app.t("Instructions are required.", "必须填写指令。"));
      const document = [
        "---",
        `name: ${skillId}`,
        `description: ${description.trim() || title.trim() || skillId}`,
        `environment: ${environment}`,
        "---",
        "",
        instructions.trim(),
        "",
      ].join("\n");
      return api.uploadMany<Skill>(
        ws("/v1/skills"),
        [{ path: "SKILL.md", blob: new Blob([document], { type: "text/markdown" }) }],
        title.trim() ? { display_title: title.trim() } : undefined,
      );
    },
    onSuccess: () => {
      onCreated();
      onClose();
    },
    onError: (cause) => setError(cause instanceof Error ? cause.message : String(cause)),
  });
  return (
    <Modal
      title={app.t("Create Skill", "在线新建 Skill")}
      onClose={onClose}
      width="min(760px, 94vw)"
      footer={<><Button onClick={onClose}>{app.t("Cancel", "取消")}</Button><Button variant="primary" disabled={create.isPending} onClick={() => create.mutate()}>{app.t("Create & publish", "创建并发布")}</Button></>}
    >
      <div className="stack">
        <div className="grid-2">
          <TextField label="Skill id" mono placeholder="release-check" value={id} onChange={(event) => setId(event.target.value)} />
          <TextField label={app.t("Display title", "显示名称")} value={title} onChange={(event) => setTitle(event.target.value)} />
        </div>
        <TextField label={app.t("Description", "描述")} value={description} onChange={(event) => setDescription(event.target.value)} />
        <div className="field">
          <label>{app.t("Execution substrate", "运行载体")}</label>
          <Segmented
            value={environment}
            onChange={setEnvironment}
            options={[
              { value: "instruction-only", label: app.t("Instruction only · no Sandbox", "纯指令 · 无需 Sandbox") },
              { value: "filesystem", label: app.t("Filesystem/scripts · Sandbox required", "文件/脚本 · 需要 Sandbox") },
            ]}
          />
          <span className="mut">{environment === "instruction-only"
            ? app.t("Runs in the Agent brain. Add supporting files later and the runtime will require a Sandbox automatically.", "在 Agent Brain 中运行；若后续添加支持文件，运行时会自动要求 Sandbox。")
            : app.t("Use when the Skill reads files, runs scripts, or needs a forked workspace.", "适用于读取文件、运行脚本或需要独立工作区的 Skill。")}</span>
        </div>
        <TextAreaField label={app.t("Instructions", "指令")} rows={12} value={instructions} onChange={(event) => setInstructions(event.target.value)} placeholder={app.t("Explain when and how the Agent should use this Skill…", "说明 Agent 应在何时、如何使用这个 Skill…")} />
        {error && <div className="err">{error}</div>}
      </div>
    </Modal>
  );
}

function SkillRuntimeCell({ skill }: { skill: Skill }) {
  const app = useApp();
  const runtime = useQuery({
    queryKey: ["skill-runtime", skill.id, skill.latest_version],
    queryFn: async () => {
      const archive = await api.bytes(ws(`/v1/skills/${encodeURIComponent(skill.id)}/versions/latest/content`));
      return skillRuntimeFromTar(new Uint8Array(archive));
    },
    staleTime: 60_000,
  });
  if (runtime.isLoading) return <span className="mut">…</span>;
  if (runtime.isError) return <Pill tone="warn">{app.t("Unknown", "未知")}</Pill>;
  return runtime.data
    ? <Pill tone="warn">{app.t("Sandbox required", "需要 Sandbox")}</Pill>
    : <Pill tone="ok">{app.t("No Sandbox", "无需 Sandbox")}</Pill>;
}

function ImportSkillModal({ onClose, onImported }: { onClose: () => void; onImported: () => void }) {
  const app = useApp();
  const [files, setFiles] = useState<File[]>([]);
  const [displayTitle, setDisplayTitle] = useState("");
  const [error, setError] = useState("");
  const folderInput = useRef<HTMLInputElement>(null);
  const zipInput = useRef<HTMLInputElement>(null);
  const publish = useMutation({
    mutationFn: async () => {
      if (files.length === 0) throw new Error(app.t("Choose a Skill folder or ZIP.", "请选择技能目录或 ZIP。"));
      const parts = files.map((file) => ({
        path: (file as File & { webkitRelativePath?: string }).webkitRelativePath || file.name,
        blob: file,
      }));
      return api.uploadMany<Skill>(ws("/v1/skills"), parts, displayTitle ? { display_title: displayTitle } : undefined);
    },
    onSuccess: () => {
      onImported();
      onClose();
    },
    onError: (cause) => setError(cause instanceof Error ? cause.message : String(cause)),
  });

  return (
    <Modal
      title={app.t("Import Skill", "导入技能")}
      onClose={onClose}
      width="min(720px, 94vw)"
      footer={<><Button onClick={onClose}>{app.t("Cancel", "取消")}</Button><Button variant="primary" disabled={publish.isPending || files.length === 0} onClick={() => publish.mutate()}>{app.t("Import", "导入")}</Button></>}
    >
      <div className="stack">
        <TextField label={app.t("Display title (optional)", "显示名称（可选）")} value={displayTitle} onChange={(event) => setDisplayTitle(event.target.value)} />
        <div className="row" style={{ gap: 8 }}>
          <Button onClick={() => folderInput.current?.click()}>{app.t("Choose folder", "选择目录")}</Button>
          <Button onClick={() => zipInput.current?.click()}>{app.t("Choose ZIP", "选择 ZIP")}</Button>
          <input {...directoryInputAttributes} ref={folderInput} type="file" multiple style={{ display: "none" }} onChange={(event) => setFiles(Array.from(event.target.files ?? []))} />
          <input ref={zipInput} type="file" accept=".zip,application/zip" style={{ display: "none" }} onChange={(event) => setFiles(Array.from(event.target.files ?? []))} />
        </div>
        <div className="mut">
          {app.t(
            "The bundle must contain one root SKILL.md. references/, scripts/, and binary assets are preserved.",
            "Bundle 必须在根目录包含一个 SKILL.md；references/、scripts/ 与二进制资源都会保留。",
          )}
        </div>
        {files.length > 0 && (
          <Card style={{ maxHeight: 240, overflow: "auto" }}>
            {files.map((file) => <div className="mono" key={`${file.name}-${file.size}`}>{(file as File & { webkitRelativePath?: string }).webkitRelativePath || file.name} <span className="mut">({file.size} B)</span></div>)}
          </Card>
        )}
        {error && <div className="err">{error}</div>}
      </div>
    </Modal>
  );
}

function SkillEditorModal({ skill, onClose, onPublished }: { skill: Skill; onClose: () => void; onPublished: () => void }) {
  const app = useApp();
  const [base, setBase] = useState<SkillVersion | null>(null);
  const [original, setOriginal] = useState<DraftFile[]>([]);
  const [files, setFiles] = useState<DraftFile[]>([]);
  const [selected, setSelected] = useState("SKILL.md");
  const [newPath, setNewPath] = useState("");
  const [loadingError, setLoadingError] = useState("");
  const addInput = useRef<HTMLInputElement>(null);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const [version, archive] = await Promise.all([
          api.get<SkillVersion>(ws(`/v1/skills/${encodeURIComponent(skill.id)}/versions/latest`)),
          api.bytes(ws(`/v1/skills/${encodeURIComponent(skill.id)}/versions/latest/content`)),
        ]);
        const loaded = skillFilesFromTar(new Uint8Array(archive));
        if (!cancelled) {
          setBase(version);
          setOriginal(loaded.map((file) => ({ ...file, bytes: file.bytes.slice() })));
          setFiles(loaded);
          setSelected(loaded.some((file) => file.path === "SKILL.md") ? "SKILL.md" : loaded[0]?.path ?? "");
        }
      } catch (cause) {
        if (!cancelled) setLoadingError(cause instanceof Error ? cause.message : String(cause));
      }
    })();
    return () => { cancelled = true; };
  }, [skill.id]);

  const selectedFile = files.find((file) => file.path === selected);
  const skillMd = files.find((file) => file.path.endsWith("SKILL.md"));
  const needsSandbox = skillNeedsSandbox(files.map((file) => file.path), skillMd && textFile(skillMd) ? decoder.decode(skillMd.bytes) : "");
  const diff = useMemo(() => {
    const before = new Map(original.map((file) => [file.path, file]));
    const after = new Map(files.map((file) => [file.path, file]));
    const added = files.filter((file) => !before.has(file.path)).map((file) => file.path);
    const deleted = original.filter((file) => !after.has(file.path)).map((file) => file.path);
    const modified = files.filter((file) => {
      const old = before.get(file.path);
      return old && (old.executable !== file.executable || !sameBytes(old.bytes, file.bytes));
    }).map((file) => file.path);
    return { added, modified, deleted, count: added.length + modified.length + deleted.length };
  }, [files, original]);

  function replaceFile(path: string, update: (file: DraftFile) => DraftFile): void {
    setFiles((current) => current.map((file) => file.path === path ? update(file) : file));
  }

  function addBrowserFiles(browserFiles: FileList | null): void {
    if (!browserFiles) return;
    void Promise.all(Array.from(browserFiles).map(async (file) => ({
      path: file.name,
      bytes: new Uint8Array(await file.arrayBuffer()),
      executable: false,
    }))).then((added) => {
      setFiles((current) => {
        const next = new Map(current.map((file) => [file.path, file]));
        for (const file of added) {
          next.set(file.path, { ...file, executable: next.get(file.path)?.executable ?? false });
        }
        return Array.from(next.values()).sort((left, right) => left.path.localeCompare(right.path));
      });
      if (added[0]) setSelected(added[0].path);
    });
  }

  const publish = useMutation({
    mutationFn: async () => {
      if (!base) throw new Error("Skill version is still loading");
      if (!files.some((file) => file.path === "SKILL.md")) throw new Error("The bundle must contain SKILL.md");
      return api.uploadMany<SkillVersion>(
        ws(`/v1/skills/${encodeURIComponent(skill.id)}/versions`),
        multipart(files),
        { executable_paths: JSON.stringify(files.filter((file) => file.executable).map((file) => file.path)) },
        { "if-match": base.version },
      );
    },
    onSuccess: () => {
      onPublished();
      onClose();
    },
    onError: (cause) => setLoadingError(cause instanceof Error ? cause.message : String(cause)),
  });

  return (
    <Modal
      title={<>{app.t("Edit Skill", "编辑技能")} · <span className="mono">{skill.id}</span></>}
      onClose={onClose}
      width="min(1040px, 96vw)"
      footer={<><span className="mut" style={{ marginRight: "auto" }}>{base ? `v${base.version} · ${diff.count} ${app.t("changes", "项变更")}` : app.t("Loading…", "加载中…")}</span><Button onClick={onClose}>{app.t("Cancel", "取消")}</Button><Button variant="primary" disabled={!base || diff.count === 0 || publish.isPending} onClick={() => publish.mutate()}>{app.t("Publish new version", "发布新版本")}</Button></>}
    >
      {loadingError && <div className="err">{loadingError}</div>}
      {base && <div className={needsSandbox ? "banner warn" : "banner info"}>
        <span>{needsSandbox ? "▣" : "◇"}</span>
        <span>{needsSandbox
          ? app.t("Sandbox required: this version declares filesystem access or includes supporting files.", "需要 Sandbox：当前版本声明了文件系统访问，或包含支持文件。")
          : app.t("Instruction-only: this version runs without creating a Sandbox.", "纯指令：当前版本无需创建 Sandbox 即可运行。")}</span>
      </div>}
      <div style={{ display: "grid", gridTemplateColumns: "minmax(220px, 30%) 1fr", gap: 12, minHeight: 460 }}>
        <Card style={{ padding: 8, overflow: "auto" }}>
          <div className="row" style={{ gap: 6, marginBottom: 8 }}>
            <Button onClick={() => addInput.current?.click()}>{app.t("Add file", "添加文件")}</Button>
            <input ref={addInput} type="file" multiple style={{ display: "none" }} onChange={(event) => addBrowserFiles(event.target.files)} />
          </div>
          <div className="row" style={{ gap: 6, marginBottom: 8 }}>
            <TextField placeholder="references/new.md" value={newPath} onChange={(event) => setNewPath(event.target.value)} />
            <Button disabled={!newPath || files.some((file) => file.path === newPath)} onClick={() => {
              const path = newPath.replaceAll("\\", "/");
              setFiles((current) => [...current, { path, bytes: new Uint8Array(), executable: false }].sort((left, right) => left.path.localeCompare(right.path)));
              setSelected(path);
              setNewPath("");
            }}>{app.t("New", "新建")}</Button>
          </div>
          {files.map((file) => (
            <button key={file.path} className={selected === file.path ? "btn primary mono" : "btn ghost mono"} style={{ width: "100%", textAlign: "left", marginBottom: 2 }} onClick={() => setSelected(file.path)}>{file.executable ? "▶ " : ""}{file.path}</button>
          ))}
        </Card>
        <div className="stack">
          {selectedFile ? (
            <>
              <TextField
                label={app.t("Path", "路径")}
                mono
                value={selectedFile.path}
                onChange={(event) => {
                  const old = selectedFile.path;
                  const path = event.target.value.replaceAll("\\", "/");
                  replaceFile(old, (file) => ({ ...file, path }));
                  setSelected(path);
                }}
                action={<Button variant="danger" disabled={selectedFile.path === "SKILL.md"} onClick={() => {
                  setFiles((current) => current.filter((file) => file.path !== selectedFile.path));
                  setSelected(files.find((file) => file.path !== selectedFile.path)?.path ?? "");
                }}>{app.t("Delete", "删除")}</Button>}
              />
              <label className="row" style={{ justifyContent: "flex-start", gap: 8 }}>
                <input type="checkbox" checked={selectedFile.executable} onChange={(event) => replaceFile(selectedFile.path, (file) => ({ ...file, executable: event.target.checked }))} />
                {app.t("Executable script", "可执行脚本")}
              </label>
              {textFile(selectedFile) ? (
                <TextAreaField mono style={{ minHeight: 350 }} value={decoder.decode(selectedFile.bytes)} onChange={(event) => replaceFile(selectedFile.path, (file) => ({ ...file, bytes: encoder.encode(event.target.value) }))} />
              ) : (
                <Card><span className="mut">{app.t("Binary file; preserved unchanged. Upload a file with the same path to replace it.", "二进制文件会原样保留；上传同路径文件可替换。")}</span></Card>
              )}
            </>
          ) : <div className="mut">{app.t("Select a file.", "请选择文件。")}</div>}
          {diff.count > 0 && <Card><strong>{app.t("Pending diff", "待发布差异")}</strong>{diff.added.map((path) => <div key={`a-${path}`} className="mono">+ {path}</div>)}{diff.modified.map((path) => <div key={`m-${path}`} className="mono">~ {path}</div>)}{diff.deleted.map((path) => <div key={`d-${path}`} className="mono">− {path}</div>)}</Card>}
        </div>
      </div>
    </Modal>
  );
}

export default function SkillsSurface() {
  const app = useApp();
  const qc = useQueryClient();
  const confirm = useConfirm();
  const toast = useToast();
  const [importing, setImporting] = useState(false);
  const [creating, setCreating] = useState(false);
  const [editing, setEditing] = useState<Skill | null>(null);
  const skills = useQuery({
    queryKey: ["skills"],
    queryFn: () => api.get<Page<Skill>>(ws("/v1/skills")),
    refetchInterval: 30_000,
  });
  const remove = useMutation({
    mutationFn: (id: string) => api.del(ws(`/v1/skills/${encodeURIComponent(id)}`)),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["skills"] });
      toast.ok(app.t("Skill deleted.", "技能已删除。"));
    },
    onError: (cause) => toast.err(cause instanceof Error ? cause.message : String(cause)),
  });
  const refresh = () => void qc.invalidateQueries({ queryKey: ["skills"] });
  const rows = skills.data?.data ?? [];

  return (
    <>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <span />
        <span className="row">
          <Button onClick={() => setImporting(true)}>{app.t("Import Skill", "导入技能")}</Button>
          <Button variant="primary" onClick={() => setCreating(true)}>+ {app.t("Create Skill", "在线新建")}</Button>
        </span>
      </div>
      {skills.error instanceof Error ? <Card><EmptyState
        title={app.t("Skills could not be loaded", "技能加载失败")}
        hint={skills.error.message}
        action={<Button onClick={() => void skills.refetch()}>{app.t("Try again", "重试")}</Button>}
      /></Card> : <Card className="responsive-table-card" style={{ padding: 0 }}>
        <table className="table">
          <thead><tr><th>{app.t("Skill", "技能")}</th><th>{app.t("Version", "版本")}</th><th>{app.t("Sandbox", "Sandbox")}</th><th /></tr></thead>
          {skills.isLoading ? <SkeletonRows rows={4} cols={4} /> : <tbody>
            {rows.map((skill) => (
              <tr key={skill.id}>
                <td data-label={app.t("Skill", "技能")}><strong>{skill.display_title ?? skill.name ?? skill.display_name ?? identifierLabel(skill.id)}</strong><TechnicalId value={skill.id} /></td>
                <td data-label={app.t("Version", "版本")} className="mut">{skill.latest_version ?? "—"}</td>
                <td data-label={app.t("Sandbox", "Sandbox")}><SkillRuntimeCell skill={skill} /></td>
                <td className="responsive-table-actions" style={{ textAlign: "right" }}><div className="row" style={{ justifyContent: "flex-end", gap: 6 }}><Button onClick={() => setEditing(skill)}>{app.t("Edit", "编辑")}</Button><Button variant="danger" disabled={remove.isPending} onClick={async () => {
                  const approved = await confirm({
                    title: app.t("Delete this Skill?", "删除该技能？"),
                    body: app.t("All published versions in this catalog entry will become unavailable to new sessions.", "该目录项中的所有已发布版本都将对新会话不可用。"),
                    confirmLabel: app.t("Delete", "删除"),
                    danger: true,
                  });
                  if (approved) remove.mutate(skill.id);
                }}>{app.t("Delete", "删除")}</Button></div></td>
              </tr>
            ))}
            {!skills.isLoading && rows.length === 0 && <tr><td colSpan={4} className="mut">{app.t("No Skills yet. Create one online or import a Skill folder.", "还没有技能。可在线创建，或导入技能目录。")}</td></tr>}
          </tbody>}
        </table>
      </Card>}
      {importing && <ImportSkillModal onClose={() => setImporting(false)} onImported={refresh} />}
      {creating && <CreateSkillModal onClose={() => setCreating(false)} onCreated={refresh} />}
      {editing && <SkillEditorModal skill={editing} onClose={() => setEditing(null)} onPublished={refresh} />}
    </>
  );
}
