// Presentational wrapper over a GateState: renders a skeleton while loading, the
// a user-facing capability state when absent, an error line on other failures,
// and delegates to `children(data)` when the face is live. Surfaces stay declarative:
//   const g = useGate<Foo>("/v1/foo");
//   return <Gate state={g} endpoint="/v1/foo">{(foo) => <FooView data={foo} />}</Gate>;

import type { ReactNode } from "react";
import type { GateState } from "../../lib/useGate";
import { useApp } from "../../lib/app-state";
import { Skeleton } from "../ui";

export default function Gate<T>({
  state,
  endpoint: _endpoint,
  note,
  action,
  children,
  loading,
}: {
  state: GateState<T>;
  /** Endpoint used by the caller to identify this capability. Never shown. */
  endpoint: string;
  note?: string;
  action?: ReactNode;
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
              "This optional capability is not available in this deployment.",
              "当前部署未提供这项可选能力。",
            )}
            {note ? <> {note}</> : null}
            {action ? <span className="row" style={{ marginTop: 10 }}>{action}</span> : null}
          </span>
        </div>
      );
    case "error":
      return <div className="err">{state.error.message}</div>;
    case "live":
      return <>{children(state.data)}</>;
  }
}
