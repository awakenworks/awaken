import { describe, expect, it } from "vitest";
import { cronScheduleLabel, deploymentRunPresentation } from "./deployments";

describe("cronScheduleLabel", () => {
  it("humanizes common schedules while preserving honest custom fallbacks", () => {
    expect(cronScheduleLabel("0 16 * * 5", "en")).toBe("Every Friday at 4:00 PM");
    expect(cronScheduleLabel("0 16 * * 5", "zh")).toBe("每周五 16:00");
    expect(cronScheduleLabel("30 8 * * *", "en")).toBe("Every day at 8:30 AM");
    expect(cronScheduleLabel("30 8 * * 1-5", "zh")).toBe("工作日 8:30");
    expect(cronScheduleLabel("*/15 * * * *", "en")).toBe("Every 15 minutes");
    expect(cronScheduleLabel("0 8 1 * *", "en")).toBe("Custom cron schedule");
    expect(cronScheduleLabel("invalid", "zh")).toBe("自定义 Cron 计划");
  });
});

describe("deploymentRunPresentation", () => {
  const run = {
    id: "deprun_1",
    deployment_id: "dep_1",
    created_at: "2026-08-29T00:00:00Z",
  };

  it("keeps failure, created Session, and in-progress launch distinct", () => {
    expect(deploymentRunPresentation({ ...run, error: { type: "agent_not_found", message: "Agent not found" }, session_id: null })).toBe("failed");
    expect(deploymentRunPresentation({ ...run, error: null, session_id: "session_1" })).toBe("session_created");
    expect(deploymentRunPresentation({ ...run, error: null, session_id: null })).toBe("starting");
  });
});
