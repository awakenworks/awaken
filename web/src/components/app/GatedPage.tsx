// Route-level gated page: probes the endpoint via useGate. Absent → the gate
// banner; live → a raw JSON view of the payload (a real, if minimal, surface)
// until a dedicated UI is built. Both the probe and the banner are shared with
// every other gated surface via useGate/<Gate>.

import { useGate } from "../../lib/useGate";
import { useApp } from "../../lib/app-state";
import { useNavigate, useParams } from "react-router";
import { Button, Card } from "../ui";
import Gate from "./Gate";

export default function GatedPage({
  title,
  endpoint,
  probe,
  note,
  fallback,
  fallbackLabel,
  fallbackLabelZh,
}: {
  title: string;
  /** Endpoint used to probe capability availability. */
  endpoint: string;
  /** The single callable path to probe (defaults to the first token of `endpoint`). */
  probe?: string;
  note?: string;
  fallback?: string;
  fallbackLabel?: string;
  fallbackLabelZh?: string;
}) {
  const app = useApp();
  const navigate = useNavigate();
  const { ws: workspace = "default" } = useParams();
  const path = probe ?? endpoint.split(/[ ·]/)[0];
  const state = useGate<unknown>(path);

  return (
    <Card>
      <h2>{title}</h2>
      <Gate
        state={state}
        endpoint={endpoint}
        note={note}
        action={fallback && fallbackLabel ? (
          <Button variant="primary" onClick={() => navigate(`/w/${workspace}/${fallback}`)}>
            {app.t(fallbackLabel, fallbackLabelZh ?? fallbackLabel)}
          </Button>
        ) : undefined}
      >
        {(data) => (
          <>
            <p className="hint" role="status">
              {app.t(
                "This capability is enabled, but this Console version does not provide a guided workflow for it. Use the API documentation for now.",
                "这项能力已经启用，但当前 Console 尚未提供引导式流程。请暂时使用 API 文档。",
              )}
            </p>
            <span className="sr-only">{JSON.stringify(data)}</span>
          </>
        )}
      </Gate>
    </Card>
  );
}
