import { Button, Pill } from "../components/ui";
import type { ConfigCapabilitiesView } from "../lib/api/types";
import { useApp } from "../lib/app-state";

export type CloudModelUiState = "loading" | "local" | "sign_in_required" | "ready" | "managed";

export interface ModelCatalogPresentation {
  showSupplyInfrastructure: boolean;
  allowSessionTest: boolean;
}

/** Hosted supply is a product catalog, not a provider-configuration surface.
 * The same capability projection controls both disclosure and actions. */
export function modelCatalogPresentation(state: CloudModelUiState): ModelCatalogPresentation {
  const managed = state === "managed";
  return {
    showSupplyInfrastructure: !managed,
    allowSessionTest: !managed,
  };
}

export function cloudModelUiState(
  capabilities: ConfigCapabilitiesView | undefined,
): CloudModelUiState {
  if (!capabilities) return "loading";
  if (!capabilities.models.cloud_models_enabled) return "local";
  if (!capabilities.models.local_catalog_enabled) return "managed";
  return capabilities.identity.authenticated ? "ready" : "sign_in_required";
}

export function CloudModelBadge({ state }: { state: CloudModelUiState }) {
  const app = useApp();
  if (state === "local") return <Pill tone="neutral">{app.t("Local · your API keys", "本地 · 自带 API Key")}</Pill>;
  if (state === "managed") return <Pill tone="agent">{app.t("Awaken Cloud · Managed", "Awaken Cloud · 托管")}</Pill>;
  if (state === "ready") return <Pill tone="agent">Awaken Cloud</Pill>;
  if (state === "sign_in_required") {
    return <Pill tone="neutral">{app.t("Cloud · sign in required", "云端 · 需要登录")}</Pill>;
  }
  return null;
}

export function CloudModelRefresh({
  state,
  pending,
  onRefresh,
}: {
  state: CloudModelUiState;
  pending: boolean;
  onRefresh: () => void;
}) {
  const app = useApp();
  if (state !== "ready") return null;
  return (
    <Button disabled={pending} onClick={onRefresh}>
      {pending
        ? app.t("Refreshing Cloud…", "正在刷新云端模型…")
        : app.t("Refresh Cloud models", "刷新云端模型")}
    </Button>
  );
}

export function CloudModelNotice({ state }: { state: CloudModelUiState }) {
  const app = useApp();
  const message =
    state === "local"
      ? app.t(
          "Cloud models are off. The catalog, provider discovery, and inference stay local and use your own keys.",
          "云端模型已关闭。目录、供应商发现和推理均保持本地，并使用你自己的密钥。",
        )
      : state === "sign_in_required"
        ? app.t(
            "Cloud models are enabled, but this deployment has no authenticated Awaken Cloud session.",
            "云端模型已启用，但当前部署尚未建立已认证的 Awaken Cloud 会话。",
          )
        : state === "managed"
          ? app.t(
              "Every available original model is listed below. Awaken Cloud manages connectivity and credentials; no setup is required.",
              "下方列出全部可用的原厂模型。连接与凭证由 Awaken Cloud 管理，无需配置。",
            )
          : null;
  if (!message) return null;
  return (
    <div className="banner info" style={{ margin: "0 16px 12px" }}>
      <span>ⓘ</span>
      <span>{message}</span>
    </div>
  );
}
