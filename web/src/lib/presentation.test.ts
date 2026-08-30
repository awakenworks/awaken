import { describe, expect, it } from "vitest";
import { credentialKindLabel, dateTimeLabel, entityDisplayName, entityDisplayTitle, identifierLabel, sessionDisplayTitle, statusLabel } from "./presentation";

describe("human-facing backend vocabulary", () => {
  it("localizes known states and removes raw enum separators for safe fallbacks", () => {
    expect(statusLabel("rescheduling", "zh")).toBe("正在重新调度");
    expect(statusLabel("pending", "en")).toBe("Pending");
    expect(statusLabel("future_state", "en")).toBe("Future state");
    expect(credentialKindLabel("worker_local", "zh")).toBe("Worker 提供");
  });

  it("keeps human names primary and turns user-chosen slugs into readable fallbacks", () => {
    expect(entityDisplayName(" Friday release review ", "Untitled Session")).toBe("Friday release review");
    expect(entityDisplayName("admin_assistant", "Unnamed Agent")).toBe("Admin assistant");
    expect(entityDisplayName("", "Untitled Session")).toBe("Untitled Session");
    expect(identifierLabel("mgmt-bootstrap")).toBe("Management bootstrap");
    expect(identifierLabel("billing_backend")).toBe("Billing backend");
    expect(identifierLabel("service-console")).toBe("Service console");
    expect(identifierLabel("codex-acp-agent")).toBe("Codex ACP agent");
    expect(identifierLabel("preview-48da0695-0138-4ebc-bd1a-f7337ed2b41b")).toBe("Agent preview");
    expect(identifierLabel("release-review-1787973667034")).toBe("Release review");
    expect(entityDisplayTitle("Assistant · Assistant", "Untitled Session")).toBe("Assistant");
    expect(entityDisplayTitle("Billing · Friday review", "Untitled Session")).toBe("Billing · Friday review");
    expect(sessionDisplayTitle(null, "preview-48da0695-0138-4ebc-bd1a-f7337ed2b41b", "en")).toBe("Agent preview");
    expect(sessionDisplayTitle(null, "agent-a", "zh")).toBe("未命名会话");
  });

  it("formats machine timestamps for people and preserves unknown source values", () => {
    expect(dateTimeLabel("2026-08-25T12:30:00Z", "en")).toMatch(/Aug 25, 2026/);
    expect(dateTimeLabel("not-a-date", "en")).toBe("not-a-date");
    expect(dateTimeLabel(null, "zh")).toBe("—");
  });
});
