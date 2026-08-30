// Workspace credential sources: one secret-in / secret-free-out materialization
// surface shared by model providers, MCP servers, and A2A remotes. Consumers bind
// the source by id; none of them owns OAuth refresh behavior.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Navigate, useNavigate } from "react-router";
import { api, workspaceFields, workspaceQuery, ws } from "../lib/api/client";
import type {
  CredentialSource,
  CredentialValidation,
  ProviderCatalog,
} from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { useConfigCapabilities } from "../lib/useConfigCapabilities";
import { Button, Card, Modal, Pill, SecretField, Skeleton, useConfirm, useToast } from "../components/ui";
import { credentialKindLabel, statusLabel } from "../lib/presentation";

const CLAUDE_CODE_SETUP_TOKEN_ENV = "CLAUDE_CODE_OAUTH_TOKEN";

const PROVIDER_NAMES: Record<string, string> = {
  anthropic: "Anthropic",
  deepseek: "DeepSeek",
  google_ai_studio: "Google AI Studio",
  openai: "OpenAI",
  openrouter: "OpenRouter",
  vertex_ai: "Vertex AI",
};

export function credentialSourceDisplayName(source: CredentialSource, locale: "en" | "zh"): string {
  if (source.provider_id) {
    const provider = PROVIDER_NAMES[source.provider_id]
      ?? source.provider_id.replaceAll("_", " ").replace(/^./, (first) => first.toUpperCase());
    return locale === "zh" ? `${provider} 模型凭证` : `${provider} model credential`;
  }
  if (source.env_key) return locale === "zh" ? `${source.env_key} 运行凭证` : `${source.env_key} runtime credential`;
  if (source.protocol_endpoint_id) return locale === "zh" ? `${source.protocol_endpoint_id} 协议凭证` : `${source.protocol_endpoint_id} protocol credential`;
  return locale === "zh" ? "Worker 提供的凭证" : "Worker-provided credential";
}

function SourceRow({ source, probeModel }: { source: CredentialSource; probeModel?: string }) {
  const app = useApp();
  const qc = useQueryClient();
  const confirm = useConfirm();
  const toast = useToast();
  const validate = useMutation({
    mutationFn: () => {
      if (!probeModel) throw new Error("No compatible active model is available");
      return api.post<CredentialValidation>(ws(`/v1/config/credentials/${source.id}/validate`), {
        workspace_id: source.workspace_id,
        model_id: probeModel,
      });
    },
  });
  const archive = useMutation({
    mutationFn: () => api.post(ws(`/v1/config/credentials/${source.id}/archive`), {
      expected_version: source.version,
    }),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["credentials"] });
      toast.ok(app.t("Credential archived.", "凭证已归档。"));
    },
    onError: (cause) => toast.err(cause instanceof Error ? cause.message : String(cause)),
  });
  const archiveSource = async () => {
    const approved = await confirm({
      title: app.t("Archive this credential?", "归档该凭证？"),
      body: app.t("New runs can no longer materialize it. The secret is not shown or copied.", "新的运行将不能再实例化它；密钥不会被显示或复制。"),
      confirmLabel: app.t("Archive", "归档"),
    });
    if (approved) archive.mutate();
  };
  const statusTone = source.status === "active" ? "ok" : "neutral";
  const isClaudeSetupToken = source.env_key === CLAUDE_CODE_SETUP_TOKEN_ENV;
  return (
    <tr>
      <td data-label={app.t("Credential", "凭证")}>
        <strong>{credentialSourceDisplayName(source, app.locale)}</strong>
        <details className="technical-id">
          <summary>{app.t("Technical ID", "技术 ID")}</summary>
          <code>{source.id}</code>
        </details>
      </td>
      <td data-label={app.t("Kind", "类型")}>
        <Pill tone="neutral">{credentialKindLabel(source.kind, app.locale)}</Pill>
      </td>
      <td data-label={app.t("Provider / environment", "供应商 / 环境")}>{source.provider_id ?? source.env_key ?? "—"}</td>
      <td data-label={app.t("Status", "状态")}>
        <Pill tone={statusTone}>{statusLabel(source.status, app.locale)}</Pill>
      </td>
      <td data-label={app.t("Last probe", "最近探针")}>
        {validate.data && (
          <Pill
            tone={validate.data.status === "valid" ? "ok" : validate.data.status === "invalid" ? "danger" : "neutral"}
          >
            {statusLabel(validate.data.status, app.locale)} · {validate.data.adapter_kind}
          </Pill>
        )}
        {validate.error instanceof Error && <span className="err">{validate.error.message}</span>}
      </td>
      <td className="responsive-table-actions" style={{ textAlign: "right", whiteSpace: "nowrap" }}>
        <Button
          variant="ghost"
          style={{ height: 26 }}
          disabled={
            validate.isPending || source.kind === "worker_local" || isClaudeSetupToken || !probeModel
          }
          onClick={() => validate.mutate()}
          title={
            probeModel
              ? app.t(`Validate with ${probeModel}`, `使用 ${probeModel} 验证`)
              : app.t("Connect a compatible model first", "请先连接兼容模型")
          }
        >
          {source.kind === "worker_local"
            ? app.t("Worker-reported", "Worker 上报")
            : isClaudeSetupToken
              ? app.t("Checked at ACP launch", "ACP 启动时校验")
              : app.t("Validate", "验证")}
        </Button>{" "}
        <Button
          variant="ghost"
          style={{ height: 26 }}
          disabled={archive.isPending || source.status !== "active"}
          onClick={() => void archiveSource()}
        >
          {archive.isPending ? app.t("Archiving…", "正在归档…") : app.t("Archive", "归档")}
        </Button>
      </td>
    </tr>
  );
}

export default function CredentialsSurface() {
  const app = useApp();
  const workspace = app.workspaceId;
  const navigate = useNavigate();
  const qc = useQueryClient();
  const capabilities = useConfigCapabilities();
  const byokEnabled = capabilities.data?.models.byok_enabled === true;
  const [addingSetupToken, setAddingSetupToken] = useState(false);
  const [setupToken, setSetupToken] = useState("");
  const sources = useQuery({
    queryKey: ["credentials", workspace],
    queryFn: () => api.get<CredentialSource[]>(ws(workspaceQuery("/v1/config/credentials", workspace))),
    enabled: byokEnabled,
  });
  const catalog = useQuery({
    queryKey: ["catalog", workspace],
    queryFn: () => api.get<ProviderCatalog>(ws("/v1/config/catalog")),
    enabled: byokEnabled,
  });
  const addSetupToken = useMutation({
    mutationFn: () =>
      api.post<CredentialSource>(ws("/v1/config/credentials"), {
        ...workspaceFields(workspace),
        kind: "vault",
        provider_id: "anthropic",
        env_key: CLAUDE_CODE_SETUP_TOKEN_ENV,
        secret: setupToken,
      }),
    onSuccess: () => {
      setSetupToken("");
      setAddingSetupToken(false);
      void qc.invalidateQueries({ queryKey: ["credentials", workspace] });
    },
  });
  if (capabilities.isLoading) return <Skeleton height={80} />;
  if (!byokEnabled) return <Navigate replace to={`/w/${workspace}/models`} />;
  return (
    <>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <span className="mut">
          {app.t("Secrets are never shown again after they are saved. This table exposes only status and last verification.", "凭证保存后不会再次显示；此表只展示状态和最近验证结果。")}
        </span>
        <span className="row">
          <Button variant="ghost" onClick={() => setAddingSetupToken(true)}>
            + {app.t("Claude Code setup token", "Claude Code setup token")}
          </Button>
          <Button variant="primary" onClick={() => navigate(`/w/${workspace}/models`)}>
            + {app.t("Connect model provider", "连接模型供应商")}
          </Button>
        </span>
      </div>
      <Card className="responsive-table-card" style={{ padding: 0 }}>
        <table className="table">
          <thead>
            <tr>
              <th>{app.t("Credential", "凭证")}</th>
              <th>{app.t("Kind", "类型")}</th>
              <th>{app.t("Provider / environment", "供应商 / 环境")}</th>
              <th>{app.t("Status", "状态")}</th>
              <th>{app.t("Last probe", "最近探针")}</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {(sources.data ?? []).map((s) => (
              <SourceRow
                key={s.id}
                source={s}
                probeModel={(catalog.data?.offerings ?? []).find(
                  (offering) =>
                    (offering.status ?? "active") === "active" &&
                    (s.provider_id == null || offering.provider_id === s.provider_id),
                )?.model_id}
              />
            ))}
            {(sources.data ?? []).length === 0 && (
              <tr>
                <td colSpan={6} className="mut">
                  {app.t("No credential sources in this workspace yet.", "工作区还没有凭证源。")}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </Card>
      {sources.error instanceof Error && <div className="err">{sources.error.message}</div>}

      {addingSetupToken && (
        <Modal
          title={app.t("Add Claude Code setup token", "添加 Claude Code setup token")}
          onClose={() => setAddingSetupToken(false)}
        >
          <p className="hint">
            {app.t(
              "Run `claude setup-token` on a trusted machine, then paste the resulting long-lived token once. It is sealed in the Vault and is available only to managed acp:claude launches as CLAUDE_CODE_OAUTH_TOKEN.",
              "在可信机器上运行 `claude setup-token`，然后仅粘贴一次生成的长期 token。它会密封进 Vault，并且只能作为 CLAUDE_CODE_OAUTH_TOKEN 提供给受管 acp:claude 运行。",
            )}
          </p>
          <SecretField
            label={app.t("Setup token (write-only)", "Setup token（仅写入）")}
            hasStored={false}
            onChange={(intent) => setSetupToken(intent.value ?? "")}
          />
          {addSetupToken.error instanceof Error && (
            <div className="err">{addSetupToken.error.message}</div>
          )}
          <div className="row" style={{ justifyContent: "flex-end" }}>
            <Button
              variant="primary"
              disabled={!setupToken || addSetupToken.isPending}
              onClick={() => addSetupToken.mutate()}
            >
              {addSetupToken.isPending
                ? app.t("Saving…", "正在保存…")
                : app.t("Save setup token", "保存 setup token")}
            </Button>
          </div>
        </Modal>
      )}
    </>
  );
}
