// Route-level gated page: probes the endpoint via useGate. Absent → the gate
// banner; live → a raw JSON view of the payload (a real, if minimal, surface)
// until a dedicated UI is built. Both the probe and the banner are shared with
// every other gated surface via useGate/<Gate>.

import { useGate } from "../../lib/useGate";
import { useApp } from "../../lib/app-state";
import { Card, JsonInspector } from "../ui";
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
            <p className="hint">
              {app.t(
                `Backend face is live (${path}). Raw payload below — a dedicated UI is pending.`,
                `后端面已就绪(${path})。下方为原始数据 —— 专用 UI 待建。`,
              )}
            </p>
            <JsonInspector value={data} />
          </>
        )}
      </Gate>
    </Card>
  );
}
