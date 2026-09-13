import { Button } from "@awaken/ui";
import { useId } from "react";
import { useQuery } from "@tanstack/react-query";
import { Link } from "react-router";
import { api, ws } from "../../lib/api/client";
import type { McpCredentialChoice } from "../../lib/api/mcp-types";
import { useApp } from "../../lib/app-state";

export default function McpCredentialSelect({ url, value, sourceKey, disabled = false, onChange }: {
  disabled?: boolean; url?: string; value: string; sourceKey?: string; onChange: (choice: McpCredentialChoice | null) => void;
}) {
  const app = useApp(); const selectionId = useId();
  const choices = useQuery({
    queryKey: ["mcp-credential-choices", app.workspaceId, url],
    queryFn: () => api.get<{ data: McpCredentialChoice[] }>(ws(`/v1/config/mcp-servers/credentials?url=${encodeURIComponent(url!)}`)),
    enabled: Boolean(url), retry: false,
  });
  const selected = value || (sourceKey ? choices.data?.data.find((c) => `${c.source.id}@${c.source.revision}` === sourceKey)?.id ?? "unavailable" : "");
  const account = choices.data?.data.find((choice) => choice.id === selected);
  return <div className="ui-stack">
    <label className="ui-field"><span>{app.t("Credential account", "凭证账户")}</span>
      <select className="ui-input" aria-label={app.t("Credential account", "凭证账户")} aria-describedby={selectionId} value={selected} disabled={disabled || !url || choices.isPending || choices.isError}
        onChange={(e) => onChange(choices.data?.data.find((c) => c.id === e.target.value) ?? null)}>
        <option value="">{app.t("No credential selected", "未选择凭证")}</option>
        {selected && !choices.data?.data.some((c) => c.id === selected) && <option value={selected}>{app.t("Selected credential unavailable", "所选凭证不可用")}</option>}
        {choices.data?.data.map((c) => <option key={c.id} value={c.id}>{c.display_name} · {c.vault_name} · {c.auth_type}</option>)}
      </select>
    </label>
    <span id={selectionId} role="status" className="mut">{url && choices.isPending
      ? app.t("Loading matching accounts…", "正在加载匹配账户…")
      : choices.isError ? app.t("Accounts could not be loaded. Use Retry accounts.", "账户加载失败，请重试账户加载。")
      : account
      ? app.t(`Selected account: ${account.display_name}. Vault: ${account.vault_name}.`, `已选账户：${account.display_name}。Vault：${account.vault_name}。`)
      : app.t(selected ? "Selected credential unavailable." : "No credential selected.", selected ? "所选凭证不可用。" : "未选择凭证。")}</span>
    {choices.error instanceof Error && <div role="alert" className="err">{choices.error.message} <Button onClick={() => void choices.refetch()}>{app.t("Retry accounts", "重试账户加载")}</Button></div>}
    {url && choices.isSuccess && !choices.data.data.length && <span role="status">{app.t("No matching accounts. Add a credential for this exact MCP URL, or test anonymously.", "没有匹配账户。请为该 MCP URL 添加凭证，或匿名测试。")}</span>}
    <span className="mut">{app.t("Matching credentials are not proof of a successful connection.", "目标匹配不代表连接已经验证成功。")}{" "}
      <Link to={`/w/${app.workspaceId}/vaults`}>{app.t("Manage credentials", "管理凭证")}</Link>
    </span>
  </div>;
}
