// Truth-driven gate: probe the real endpoint instead of a static placeholder.
// Absent (404/405) → "backend face not mounted"; present → a raw JSON view of the
// payload (a real, if minimal, surface) until a dedicated UI is built. This turns
// the IA's gated pages from dead placeholders into self-revealing surfaces.

import { useQuery } from "@tanstack/react-query";
import { api, isAbsent, ws } from "../../lib/api/client";
import { useApp } from "../../lib/app-state";
import { Card, JsonInspector, Skeleton } from "../ui";

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
  const query = useQuery({
    queryKey: ["gated", path],
    queryFn: () => api.get<unknown>(ws(path)),
    retry: (n, err) => !isAbsent(err) && n < 1,
  });

  return (
    <Card>
      <h2>{title}</h2>
      {query.isLoading ? (
        <Skeleton height={60} />
      ) : query.isError && isAbsent(query.error) ? (
        <div className="banner gate">
          <span>◌</span>
          <span>
            {app.t("This surface is designed but its backend face is not mounted yet: ", "该界面已设计,后端面尚未就绪:")}
            <code>{endpoint}</code>
            {note ? ` — ${note}` : null}{" "}
            {app.t("See design/web-ui.md §7 for the roadmap.", "路线见 design/web-ui.md §7。")}
          </span>
        </div>
      ) : query.isError ? (
        <div className="err">{query.error instanceof Error ? query.error.message : "error"}</div>
      ) : (
        <>
          <p className="hint">
            {app.t(
              `Backend face is live (${path}). Raw payload below — a dedicated UI is pending.`,
              `后端面已就绪(${path})。下方为原始数据 —— 专用 UI 待建。`,
            )}
          </p>
          <JsonInspector value={query.data} />
        </>
      )}
    </Card>
  );
}
