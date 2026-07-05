import { useApp } from "../lib/app-state";

export default function InboxSurface() {
  const app = useApp();
  return (
    <>
      <p className="mut" style={{ margin: 0 }}>
        {app.t(
          "Everything waiting on a human decision — across every project.",
          "所有等待人工决策的事项——跨越每个项目。",
        )}
      </p>
      <div className="banner gate">
        <span>◌</span>
        <span>
          {app.t(
            "The approvals feed needs the needs-attention aggregation (roadmap §7.2). Tool confirmations already work inline in each session's transcript.",
            "审批流需要 needs-attention 聚合端点(路线 §7.2);工具确认目前已可在各会话转录中行内完成。",
          )}
        </span>
      </div>
    </>
  );
}
