import type { ReactNode } from "react";
import { Navigate, useParams } from "react-router";
import type { ConfigCapabilitiesView } from "../../lib/api/types";
import { hasSurface } from "../../lib/navigation/paths";
import { useConfigCapabilities } from "../../lib/useConfigCapabilities";
import { Card, Skeleton } from "../ui";

export default function SurfaceRoute({
  surface,
  children,
}: {
  surface: keyof ConfigCapabilitiesView["surfaces"];
  children: ReactNode;
}) {
  const capabilities = useConfigCapabilities();
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
    return <Navigate to={`/w/${encodeURIComponent(ws)}/overview`} replace />;
  }
  return <>{children}</>;
}
