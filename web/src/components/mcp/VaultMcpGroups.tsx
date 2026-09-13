import { useQuery } from "@tanstack/react-query";
import { Button, Card } from "@awaken/ui";
import { api, ws } from "../../lib/api/client";
import type { McpConfigurationView, McpCredentialChoice } from "../../lib/api/mcp-types";
import { useApp } from "../../lib/app-state";

export default function VaultMcpGroups({ onOpenVault }: { onOpenVault: (id: string) => void }) {
  const app = useApp();
  const inventory = useQuery({ queryKey: ["mcp-credential-inventory", app.workspaceId], queryFn: () => api.get<{ data: McpCredentialChoice[] }>(ws("/v1/config/mcp-servers/credential-inventory")) });
  const catalog = useQuery({ queryKey: ["mcp-configuration", app.workspaceId], queryFn: () => api.get<McpConfigurationView>(ws("/v1/config/mcp-servers")) });
  const groups = new Map<string, McpCredentialChoice[]>();
  for (const choice of inventory.data?.data ?? []) groups.set(choice.target_fingerprint, [...(groups.get(choice.target_fingerprint) ?? []), choice]);
  return <div className="ui-stack mcp-config">
    <p className="hint">{app.t("Active MCP accounts grouped by their exact target. This view includes custom endpoints outside the catalog. Other credential types remain available in the Vault view.", "按确切目标分组展示可用 MCP 账户，包括目录之外的自定义地址。其他凭证类型仍在 Vault 视图管理。")}</p>
    {inventory.error instanceof Error && <div role="alert" className="err">{inventory.error.message}<Button onClick={() => void inventory.refetch()}>{app.t("Retry accounts", "重试账户加载")}</Button></div>}
    {[...groups].map(([fingerprint, choices]) => {
      const server = catalog.data?.servers.find((row) => row.target_fingerprint === fingerprint);
      return <Card key={fingerprint} className="ui-stack">
        <strong>{server?.definition.name ?? choices[0].mcp_server_url}</strong><code>{choices[0].mcp_server_url}</code>
        {choices.map((choice) => <div className="row" key={choice.id}>
          <span>{choice.display_name} · {choice.auth_type} · {choice.vault_name}</span>
          <Button variant="ghost" onClick={() => onOpenVault(choice.vault_id)}>{app.t("Open Vault", "打开 Vault")}</Button>
        </div>)}
      </Card>;
    })}
    {inventory.isSuccess && !groups.size && <p>{app.t("No active MCP credentials.", "暂无可用的 MCP 凭证。")}</p>}
  </div>;
}
