// @formal surface requirement=product.admin_console.console_workflows
// @formal reason: The Quickstart integrate step's notice pointing at the SDK example; it renders guidance
// @formal reason: and owns no configuration. Classification only, not formal evidence.
// Step 4 of the Quickstart arc, rendered on the durable Session route. The
// editor owns steps 1–3; this completes the rail where the work actually landed
// and hands over the one integration the operator needs next. It is guidance
// driven entirely by the presentational `from=quickstart` query, so dismissing
// it changes nothing about the Session.

import { ArrowRight, Check, X } from "@awaken/ui/icons";
import { InlineNotice, Button } from "@awaken/ui";
import { Link } from "react-router";
import QuickstartProgress from "../agent/QuickstartProgress";
import { CopyButton } from "../ui";
import { useApp } from "../../lib/app-state";
import { protocolHelpPath } from "../../lib/navigation/paths";
import { managedSdkExample } from "../../lib/sdk-examples";

export default function QuickstartIntegrateNotice({
  workspaceId,
  agentId,
  environmentId,
  onDismiss,
}: {
  workspaceId: string;
  agentId?: string;
  environmentId?: string | null;
  onDismiss: () => void;
}) {
  const app = useApp();
  // The same template the protocol directory shows, with this Session's own
  // execution coordinates filled in. Copy-time convenience only: the snippet is
  // never configuration authority.
  const snippet = managedSdkExample({ agentId, environmentId, workspaceId });
  return (
    <InlineNotice
      tone="info"
      icon={<Check />}
      actions={
        <Button
          variant="ghost"
          aria-label={app.t("Dismiss Quickstart guidance", "关闭 Quickstart 引导")}
          onClick={onDismiss}
        >
          <X />
        </Button>
      }
    >
      <span style={{ flex: 1, minWidth: 0 }}>
        <QuickstartProgress current="integrate" />
        {app.t(
          "The Agent is published and its first task was submitted to this durable Session. Review the live result below, then connect the same Agent from your trusted backend with the Managed Agents SDK.",
          "Agent 已发布，首次任务也已提交到这个持久 Session。先在下方检查实时结果，再通过可信后端使用 Managed Agents SDK 接入同一个 Agent。",
        )}{" "}
        <Link
          className="protocol-help-link"
          to={protocolHelpPath(workspaceId, "managed", { agentId, environmentId })}
        >
          {app.t("Open Managed Agents SDK setup", "打开 Managed Agents SDK 配置")} <ArrowRight />
        </Link>
        <details>
          <summary style={{ cursor: "pointer", marginTop: 8 }}>
            {app.t("Export as code", "导出为代码")}
          </summary>
          <div className="row" style={{ justifyContent: "flex-end", marginTop: 6 }}>
            <CopyButton
              value={snippet}
              label={app.t("copy", "复制")}
              copiedLabel={app.t("copied", "已复制")}
            />
          </div>
          <pre className="quickstart-code">{snippet}</pre>
        </details>
      </span>
    </InlineNotice>
  );
}
