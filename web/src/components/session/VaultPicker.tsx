import { CheckPicker } from "@awaken/ui";
import { useQuery } from "@tanstack/react-query";
import { api, ws } from "../../lib/api/client";
import type { Page, Vault } from "../../lib/api/types";
import { useApp } from "../../lib/app-state";
export default function VaultPicker({ selected, onChange }: { selected: string[]; onChange: (ids: string[]) => void }) {
  const app = useApp();
  const vaults = useQuery({ queryKey: ["vaults", app.workspaceId], queryFn: () => api.get<Page<Vault>>(ws("/v1/vaults")) });
  return <div className="ui-field">
    <label>{app.t("Runtime Secret Vaults for this Session", "本次会话使用的运行时凭证 Vault")}</label>
    <span className="mut">{app.t("Vault order selects the first matching MCP account. Review overlapping accounts before running.", "同一 MCP 地址按 Vault 顺序选取首个匹配账户，运行前请检查重复账户。")}</span>
    <CheckPicker selected={selected} onChange={onChange} options={(vaults.data?.data ?? []).filter((v) => !v.archived_at).map((v) => ({ id: v.id, label: v.display_name || v.id, description: <small className="mono mut">{v.id}</small> }))}
      empty={vaults.isPending ? null : app.t("No Runtime Secret Vaults are available.", "没有可用的运行时凭证 Vault。")} />
    {vaults.error instanceof Error && <div className="err">{vaults.error.message}</div>}
  </div>;
}
