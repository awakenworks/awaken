// Project · Skills: the runtime delivered-skill catalog the host offers on
// every thread (Managed Agents `/v1/skills`). Authoring/registration is a
// multipart upload (CLI/SDK); this read-mostly page lists and deletes.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../lib/api/client";
import type { Page, Skill } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { Button, Card } from "../components/ui";

export default function SkillsSurface() {
  const app = useApp();
  const qc = useQueryClient();
  const skills = useQuery({
    queryKey: ["skills"],
    queryFn: () => api.get<Page<Skill>>("/v1/skills"),
    refetchInterval: 30_000,
  });
  const remove = useMutation({
    mutationFn: (id: string) => api.del(`/v1/skills/${id}`),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["skills"] }),
  });
  const rows = skills.data?.data ?? [];

  return (
    <>
      <span className="mut">
        {app.t(
          "Skills are the runtime delivered-skill catalog the host offers on every thread; authoring/registration is via multipart upload (CLI/SDK).",
          "技能是宿主在每个线程上提供的运行时交付技能目录;创作/注册通过 multipart 上传(CLI/SDK)完成。",
        )}
      </span>
      <div className="banner gate">
        <span>◌</span>
        {app.t(
          "Skill create/version upload is multipart (SDK/CLI); this page lists and deletes.",
          "技能创建/版本上传为 multipart(SDK/CLI);此页面用于列出与删除。",
        )}
      </div>
      {skills.error instanceof Error && <div className="err">{skills.error.message}</div>}
      <Card style={{ padding: 0 }}>
        <table className="table">
          <thead>
            <tr>
              <th>{app.t("Skill", "技能")}</th>
              <th>{app.t("Name", "名称")}</th>
              <th>{app.t("Description", "描述")}</th>
              <th>{app.t("Version", "版本")}</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {rows.map((s) => (
              <tr key={s.id}>
                <td className="mono">{s.id}</td>
                <td>{s.name ?? s.display_name ?? s.id}</td>
                <td className="mut">{s.description ?? "—"}</td>
                <td className="mut">{s.latest_version ?? "—"}</td>
                <td style={{ textAlign: "right" }}>
                  <Button
                    variant="danger"
                    style={{ height: 22 }}
                    disabled={remove.isPending}
                    onClick={() => remove.mutate(s.id)}
                  >
                    {app.t("Delete", "删除")}
                  </Button>
                </td>
              </tr>
            ))}
            {rows.length === 0 && (
              <tr>
                <td colSpan={5} className="mut">
                  {skills.isLoading ? "…" : app.t("No skills delivered yet.", "尚无已交付技能。")}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </Card>
    </>
  );
}
