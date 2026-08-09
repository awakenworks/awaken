import type { AgentConfig, InputBinding } from "../../lib/api/types";
import { useApp } from "../../lib/app-state";
import { Button, Card, Modal } from "../ui";
import ConfigDiff from "./ConfigDiff";
import PublicationSnapshotSummary from "./PublicationSnapshotSummary";

export interface QuickRunIntent {
  environmentId?: string;
  task: string;
}

export default function AgentPublicationModals({
  config,
  baseline,
  resources,
  resourceRevision,
  quickRunIntent,
  quickRunPending,
  quickRunError,
  showPublish,
  publishPending,
  onCloseQuickRun,
  onConfirmQuickRun,
  onClosePublish,
  onConfirmPublish,
}: {
  config: AgentConfig;
  baseline: Partial<AgentConfig>;
  resources: InputBinding[];
  resourceRevision: number;
  quickRunIntent?: QuickRunIntent;
  quickRunPending: boolean;
  quickRunError?: Error;
  showPublish: boolean;
  publishPending: boolean;
  onCloseQuickRun: () => void;
  onConfirmQuickRun: (intent: QuickRunIntent) => void;
  onClosePublish: () => void;
  onConfirmPublish: () => void;
}) {
  const app = useApp();
  const { generation: _generation, ...reviewedConfig } = config;
  const { generation: _baselineGeneration, ...reviewedBaseline } = baseline;
  return (
    <>
      {quickRunIntent && (
        <Modal
          title={app.t("Review the first real run", "审阅首次真实运行")}
          width="min(780px, 92vw)"
          onClose={() => !quickRunPending && onCloseQuickRun()}
          footer={
            <>
              <Button disabled={quickRunPending} onClick={onCloseQuickRun}>{app.t("Cancel", "取消")}</Button>
              <Button variant="primary" disabled={quickRunPending} onClick={() => onConfirmQuickRun(quickRunIntent)}>
                {quickRunPending
                  ? app.t("Publishing & starting…", "正在发布并启动…")
                  : app.t("Publish & run", "发布并运行")} ➤
              </Button>
            </>
          }
        >
          <div className="banner info">
            <span>ⓘ</span>
            <span>{app.t(
              "This explicit checkpoint saves and validates the draft, publishes an immutable snapshot, creates a durable Session, and sends the first task.",
              "此确认会保存并校验草稿、发布不可变快照、创建持久 Session，并发送首次任务。",
            )}</span>
          </div>
          <PublicationSnapshotSummary
            sourceRevision={config.generation}
            resourceRevision={resourceRevision}
            resources={resources}
          />
          <Card style={{ marginTop: 12 }}>
            <div><strong>{app.t("Environment", "Environment")}</strong> · <code>{quickRunIntent.environmentId ?? "default"}</code></div>
            <div style={{ marginTop: 8 }}><strong>{app.t("First task", "首次任务")}</strong></div>
            <p style={{ whiteSpace: "pre-wrap" }}>{quickRunIntent.task}</p>
          </Card>
          <div style={{ marginTop: 14 }}>
            <strong>{app.t("Agent configuration changes", "Agent 配置差异")}</strong>
            <div style={{ marginTop: 8 }}><ConfigDiff before={reviewedBaseline} after={reviewedConfig} /></div>
          </div>
          {quickRunError && <div className="err">{quickRunError.message}</div>}
        </Modal>
      )}

      {showPublish && (
        <Modal
          title={app.t("Publish changes?", "发布改动？")}
          width="min(760px, 92vw)"
          onClose={onClosePublish}
          footer={
            <>
              <Button onClick={onClosePublish}>{app.t("Cancel", "取消")}</Button>
              <Button variant="primary" disabled={publishPending} onClick={onConfirmPublish}>
                {app.t("Publish", "发布")} ➤
              </Button>
            </>
          }
        >
          <div className="banner ok">
            <span>✓</span>
            <span>{app.t(
              "Draft compiled successfully. Publishing is the explicit live checkpoint.",
              "草稿已通过编译。发布是明确的上线检查点。",
            )}</span>
          </div>
          <PublicationSnapshotSummary
            sourceRevision={config.generation}
            resourceRevision={resourceRevision}
            resources={resources}
          />
          <div style={{ marginTop: 16 }}>
            <strong>{app.t("Agent configuration changes", "Agent 配置差异")}</strong>
            <div style={{ marginTop: 10 }}><ConfigDiff before={reviewedBaseline} after={reviewedConfig} /></div>
          </div>
        </Modal>
      )}
    </>
  );
}
