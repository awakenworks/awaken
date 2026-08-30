import type { ReactNode } from "react";
import { useNavigate, useParams } from "react-router";
import type { ConfigCapabilitiesView } from "../../lib/api/types";
import { hasSurface } from "../../lib/navigation/paths";
import { useConfigCapabilities } from "../../lib/useConfigCapabilities";
import { useApp } from "../../lib/app-state";
import { Button, Card, Skeleton } from "../ui";

export default function SurfaceRoute({
  surface,
  children,
}: {
  surface: keyof ConfigCapabilitiesView["surfaces"];
  children: ReactNode;
}) {
  const capabilities = useConfigCapabilities();
  const app = useApp();
  const navigate = useNavigate();
  const { ws = "default" } = useParams();

  if (capabilities.isPending) return <Skeleton height={80} />;
  if (capabilities.isError) {
    return (
      <Card>
        <div className="err">{capabilities.error.message}</div>
      </Card>
    );
  }
  if (!hasSurface(capabilities.data, surface)) {
    return (
      <Card className="capability-unavailable">
        <span className="readiness-icon attention" aria-hidden="true">!</span>
        <div>
          <h2>{app.t("Not available in this deployment", "当前部署未开放此能力")}</h2>
          <p className="mut">{app.t(
            "This URL is valid, but the running Awaken role does not expose this capability. No data was changed.",
            "此页面地址有效，但当前 Awaken 运行角色没有开放该能力。本次访问未修改任何数据。",
          )}</p>
          <span className="row">
            <Button variant="primary" onClick={() => navigate(`/w/${encodeURIComponent(ws)}/overview`)}>{app.t("Return to overview", "返回概览")}</Button>
            <Button onClick={() => navigate(`/w/${encodeURIComponent(ws)}/settings`)}>{app.t("Review available configuration", "查看可用配置")}</Button>
          </span>
        </div>
      </Card>
    );
  }
  return <>{children}</>;
}
