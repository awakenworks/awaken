// Project · Environments: the reusable container templates sessions run in
// (Managed Agents `/v1/environments`). A session references one by
// `environment_id`. cloud = Anthropic-hosted; self_hosted = your own worker.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { api } from "../lib/api/client";
import type { Environment, Page } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { Button, Card, Pill, TextField } from "../components/ui";

function CreateModal({ onClose }: { onClose: () => void }) {
  const app = useApp();
  const qc = useQueryClient();
  const [name, setName] = useState("");
  const [kind, setKind] = useState<"cloud" | "self_hosted">("cloud");
  const [net, setNet] = useState<"unrestricted" | "limited">("unrestricted");
  const [hosts, setHosts] = useState("");
  const create = useMutation({
    mutationFn: () =>
      api.post<Environment>("/v1/environments", {
        name: name || "environment",
        config: {
          type: kind,
          ...(kind === "cloud"
            ? {
                networking:
                  net === "limited"
                    ? {
                        type: "limited",
                        allowed_hosts: hosts.split(",").map((h) => h.trim()).filter(Boolean),
                        allow_mcp_servers: true,
                      }
                    : { type: "unrestricted" },
              }
            : {}),
        },
      }),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["environments"] });
      onClose();
    },
  });
  return (
    <div className="overlay" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>{app.t("New environment", "新建运行环境")}</h3>
        <TextField label={app.t("Name", "名称")} value={name} onChange={(e) => setName(e.target.value)} placeholder="my-dev-env" />
        <div className="field">
          <label>{app.t("Runtime", "运行时")}</label>
          <div className="row">
            {(["cloud", "self_hosted"] as const).map((k) => (
              <Button key={k} variant={kind === k ? "primary" : "ghost"} onClick={() => setKind(k)}>
                {k}
              </Button>
            ))}
          </div>
          <span className="mut">
            {kind === "cloud"
              ? app.t("Anthropic-hosted container.", "Anthropic 托管容器。")
              : app.t("Your own worker polls the work queue (networking is yours).", "你自己的 worker 拉取工作队列(egress 由你控制)。")}
          </span>
        </div>
        {kind === "cloud" && (
          <div className="field">
            <label>{app.t("Networking", "网络")}</label>
            <div className="row">
              {(["unrestricted", "limited"] as const).map((n) => (
                <Button key={n} variant={net === n ? "primary" : "ghost"} onClick={() => setNet(n)}>
                  {n}
                </Button>
              ))}
            </div>
            {net === "limited" && (
              <input
                className="input mono"
                placeholder="api.example.com, *.foo.com"
                value={hosts}
                onChange={(e) => setHosts(e.target.value)}
              />
            )}
          </div>
        )}
        {create.error instanceof Error && <div className="err">{create.error.message}</div>}
        <div className="row" style={{ justifyContent: "flex-end" }}>
          <Button onClick={onClose}>
            {app.t("Cancel", "取消")}
          </Button>
          <Button variant="primary" disabled={create.isPending} onClick={() => create.mutate()}>
            {app.t("Create", "创建")}
          </Button>
        </div>
      </div>
    </div>
  );
}

export default function EnvironmentsSurface() {
  const app = useApp();
  const qc = useQueryClient();
  const [creating, setCreating] = useState(false);
  const envs = useQuery({
    queryKey: ["environments"],
    queryFn: () => api.get<Page<Environment>>("/v1/environments"),
    refetchInterval: 30_000,
  });
  const archive = useMutation({
    mutationFn: (id: string) => api.post(`/v1/environments/${id}/archive`),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["environments"] }),
  });
  const rows = envs.data?.data ?? [];

  return (
    <>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <span className="mut">
          {app.t(
            "Reusable container templates. Sessions reference one by environment_id.",
            "可复用的容器模板。会话按 environment_id 引用。",
          )}
        </span>
        <Button variant="primary" onClick={() => setCreating(true)}>
          + {app.t("New environment", "新建环境")}
        </Button>
      </div>
      {envs.error instanceof Error && <div className="err">{envs.error.message}</div>}
      <Card style={{ padding: 0 }}>
        <table className="table">
          <thead>
            <tr>
              <th>Environment</th>
              <th>{app.t("Name", "名称")}</th>
              <th>{app.t("Runtime", "运行时")}</th>
              <th>{app.t("Networking", "网络")}</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {rows.map((e) => (
              <tr key={e.id}>
                <td className="mono">{e.id}</td>
                <td>{e.name}</td>
                <td>
                  <Pill tone={e.config.type === "self_hosted" ? "agent" : "neutral"}>
                    {e.config.type}
                  </Pill>
                </td>
                <td className="mut">{e.config.networking?.type ?? "—"}</td>
                <td style={{ textAlign: "right" }}>
                  {e.archived_at ? (
                    <Pill tone="neutral">{app.t("archived", "已归档")}</Pill>
                  ) : (
                    <Button
                      variant="ghost"
                      style={{ height: 22 }}
                      disabled={archive.isPending}
                      onClick={() => archive.mutate(e.id)}
                    >
                      {app.t("Archive", "归档")}
                    </Button>
                  )}
                </td>
              </tr>
            ))}
            {rows.length === 0 && (
              <tr>
                <td colSpan={5} className="mut">
                  {envs.isLoading ? "…" : app.t("No environments yet.", "还没有运行环境。")}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </Card>
      {creating && <CreateModal onClose={() => setCreating(false)} />}
    </>
  );
}
