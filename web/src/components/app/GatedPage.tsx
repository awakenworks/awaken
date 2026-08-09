// Route-level gated page: probes the endpoint via useGate. Absent → the gate
// banner; live → a raw JSON view of the payload (a real, if minimal, surface)
// until a dedicated UI is built. Both the probe and the banner are shared with
// every other gated surface via useGate/<Gate>.

import { useGate } from "../../lib/useGate";
import { useApp } from "../../lib/app-state";
import { Card } from "../ui";
import Gate from "./Gate";

export default function GatedPage({
  title,
  endpoint,
  probe,
  note,
}: {
  title: string;
  /** Human-readable endpoint(s) shown in the gate banner. */
  endpoint: string;
  /** The single callable path to probe (defaults to the first token of `endpoint`). */
  probe?: string;
  note?: string;
}) {
  const app = useApp();
  const path = probe ?? endpoint.split(/[ ·]/)[0];
  const state = useGate<unknown>(path);

  return (
    <Card>
      <h2>{title}</h2>
      <Gate state={state} endpoint={endpoint} note={note}>
        {(data) => (
          <>
            <p className="hint" role="status">
              {app.t(
                `This optional capability is enabled at ${path}, but this Console version does not yet provide a safe guided workflow for it. Use the API documentation instead of editing raw data here.`,
                `可选能力已在 ${path} 启用，但当前 Console 尚未提供安全的引导式流程。请使用 API 文档，不要在此直接编辑原始数据。`,
              )}
            </p>
            <span className="sr-only">{JSON.stringify(data)}</span>
          </>
        )}
      </Gate>
    </Card>
  );
}
