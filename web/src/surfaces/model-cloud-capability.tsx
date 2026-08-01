import { Button, Pill } from "../components/ui";
import type { ConfigCapabilitiesView } from "../lib/api/types";
import { useApp } from "../lib/app-state";

export type CloudModelUiState = "loading" | "local" | "sign_in_required" | "ready" | "managed";

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
  if (state === "local") return <Pill tone="neutral">Local · BYOK</Pill>;
  if (state === "managed") return <Pill tone="agent">Awaken Cloud · Managed</Pill>;
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
              "Models, Provider routes, and credentials are managed by Awaken Cloud. Choose a Provider-native model; no Provider key is required.",
              "模型、供应商路由和凭证由 Awaken Cloud 托管。请选择原厂模型，无需提供供应商密钥。",
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
