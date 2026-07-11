// Presentational wrapper over a GateState: renders a skeleton while loading, the
// "designed but not mounted" banner when absent, an error line on other failures,
// and delegates to `children(data)` when the face is live. Surfaces stay declarative:
//   const g = useGate<Foo>("/v1/foo");
//   return <Gate state={g} endpoint="/v1/foo">{(foo) => <FooView data={foo} />}</Gate>;

import type { ReactNode } from "react";
import type { GateState } from "../../lib/useGate";
import { useApp } from "../../lib/app-state";
import { Skeleton } from "../ui";

export default function Gate<T>({
  state,
  endpoint,
  note,
  children,
  loading,
}: {
  state: GateState<T>;
  /** Human-readable endpoint shown in the gate banner. */
  endpoint: string;
  note?: string;
  children: (data: T) => ReactNode;
  /** Custom loading node (default: a skeleton). */
  loading?: ReactNode;
}) {
  const app = useApp();
  switch (state.status) {
    case "loading":
      return <>{loading ?? <Skeleton height={60} />}</>;
    case "absent":
      return (
        <div className="banner gate">
          <span>◌</span>
          <span>
            {app.t(
              "This surface is designed but its backend face is not mounted yet: ",
              "该界面已设计,后端面尚未就绪:",
            )}
            <code>{endpoint}</code>
            {note ? ` — ${note}` : null}{" "}
            {app.t("See design/web-ui.md §7 for the roadmap.", "路线见 design/web-ui.md §7。")}
          </span>
        </div>
      );
    case "error":
      return <div className="err">{state.error.message}</div>;
    case "live":
      return <>{children(state.data)}</>;
  }
}
