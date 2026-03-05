import { test, expect } from "@playwright/test";

test.describe("AI SDK Chat", () => {
  test("page renders with heading, input, and send button", async ({
    page,
  }) => {
    await page.goto("/");

    await expect(page.getByTestId("page-title")).toContainText("AI SDK Framework Integration Playground", {
      timeout: 15_000,
    });

    // Input and button should be present.
    await expect(page.getByPlaceholder("Type a message...")).toBeVisible();
    await expect(page.getByRole("button", { name: "Send" })).toBeVisible();
  });

  test("send message and receive streaming response", async ({ page }) => {
    await page.goto("/");

    await expect(page.getByTestId("page-title")).toContainText("AI SDK Framework Integration Playground", {
      timeout: 15_000,
    });

    const input = page.getByPlaceholder("Type a message...");
    await input.fill("What is 2+2? Reply with just the number.");
    await page.getByRole("button", { name: "Send" }).click();

    await expect(page.locator("main")).toContainText("2+2", {
      timeout: 10_000,
    });

    // Wait for streaming to finish (Send button re-enabled), then check content.
    const sendButton = page.getByRole("button", { name: "Send" });
    await expect(sendButton).toBeEnabled({ timeout: 30_000 });

    await expect(page.locator("main")).toContainText("4", { timeout: 10_000 });
  });

  test("multi-turn conversation preserves history", async ({ page }) => {
    await page.goto("/");

    await expect(page.getByTestId("page-title")).toContainText("AI SDK Framework Integration Playground", {
      timeout: 15_000,
    });

    const input = page.getByPlaceholder("Type a message...");

    // Turn 1: ask a question.
    await input.fill("Remember the number 7.");
    await page.getByRole("button", { name: "Send" }).click();

    const sendButton = page.getByRole("button", { name: "Send" });
    await expect(sendButton).toBeEnabled({ timeout: 45_000 });

    // Turn 2: follow-up referencing the first message.
    await input.fill(
      "What number did I just ask you to remember? Reply with just the number."
    );
    await page.getByRole("button", { name: "Send" }).click();
    await expect(sendButton).toBeEnabled({ timeout: 45_000 });
    await expect(page.locator("main")).toContainText("Remember the number 7.", {
      timeout: 10_000,
    });
    await expect(page.locator("main")).toContainText("7", { timeout: 10_000 });
  });

  test("history messages survive page reload", async ({ page }) => {
    await page.goto("/");

    await expect(page.getByTestId("page-title")).toContainText("AI SDK Framework Integration Playground", {
      timeout: 15_000,
    });

    const input = page.getByPlaceholder("Type a message...");

    // Send a message and wait for the agent response.
    await input.fill("Remember the fruit: pineapple. Just say OK.");
    await page.getByRole("button", { name: "Send" }).click();

    // Wait for streaming to finish (Send button re-enabled).
    const sendButton = page.getByRole("button", { name: "Send" });
    await expect(sendButton).toBeEnabled({ timeout: 15_000 });
    await expect(page.locator("main")).toContainText("pineapple", {
      timeout: 10_000,
    });

    // Reload the page — history should be restored from the backend.
    await page.reload();

    await expect(page.getByTestId("page-title")).toContainText("AI SDK Framework Integration Playground", {
      timeout: 15_000,
    });

    // Verify the original user message content is present after restore.
    await expect(page.locator("main")).toContainText("pineapple", {
      timeout: 15_000,
    });
  });

  test("displays token usage metrics after response", async ({ page }) => {
    await page.goto("/");

    await expect(page.getByTestId("page-title")).toContainText("AI SDK Framework Integration Playground", {
      timeout: 15_000,
    });

    const input = page.getByPlaceholder("Type a message...");
    await input.fill("What is 2+2? Reply with just the number.");
    await page.getByRole("button", { name: "Send" }).click();

    // Wait for streaming to finish.
    const sendButton = page.getByRole("button", { name: "Send" });
    await expect(sendButton).toBeEnabled({ timeout: 15_000 });

    // The metrics panel should appear with token usage data.
    const metricsPanel = page.getByTestId("metrics-panel");
    await expect(metricsPanel).toBeVisible({ timeout: 10_000 });

    // Should contain at least one metrics entry with token count.
    const entry = page.getByTestId("metrics-entry").first();
    await expect(entry).toBeVisible();
    await expect(entry).toContainText("tokens");
  });

  test("handles tool execution error gracefully", async ({ page }) => {
    await page.goto("/");

    await expect(page.getByTestId("page-title")).toContainText("AI SDK Framework Integration Playground", {
      timeout: 15_000,
    });

    const input = page.getByPlaceholder("Type a message...");
    await input.fill("Trigger an error by using the failingTool.");
    await page.getByRole("button", { name: "Send" }).click();
    await expect(page.getByRole("button", { name: "Send" })).toBeEnabled({
      timeout: 45_000,
    });

    // The tool error should be displayed in the chat.
    // The AI SDK frontend renders tool.errorText in a red div with "Error:" prefix.
    // Or the agent may describe the error in text. Either way, "fail" should appear.
    const chatArea = page.locator("main");
    await expect(chatArea).toContainText(/fail|error/i, { timeout: 15_000 });
  });

  test("multi-round tool execution with tool display", async ({ page }) => {
    await page.goto("/");

    await expect(page.getByTestId("page-title")).toContainText("AI SDK Framework Integration Playground", {
      timeout: 15_000,
    });

    const input = page.getByPlaceholder("Type a message...");
    await input.fill("Use the serverInfo tool and tell me the server name.");
    await page.getByRole("button", { name: "Send" }).click();

    // Wait for streaming to finish.
    const sendButton = page.getByRole("button", { name: "Send" });
    await expect(sendButton).toBeEnabled({ timeout: 30_000 });

    const chatArea = page.locator("main");

    // Tool display: the frontend should render the tool call UI.
    await expect(chatArea).toContainText("Tool: serverInfo", {
      timeout: 10_000,
    });

    // Tool output should contain the server name.
    await expect(chatArea).toContainText("tirea-agentos", {
      timeout: 10_000,
    });
  });

  test("permission approval allows backend tool execution", async ({ page }) => {
    await page.route("**/api/history?**", async (route) => {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({ messages: [] }),
      });
    });

    let chatRequestCount = 0;
    let resumeRequestBody: Record<string, unknown> | null = null;

    await page.route("**/api/chat", async (route) => {
      chatRequestCount += 1;
      const payloadText = route.request().postData() ?? "{}";
      const payload = JSON.parse(payloadText) as Record<string, unknown>;

      if (chatRequestCount === 1) {
        const sse = [
          'data: {"type":"start","messageId":"m_perm_1"}',
          "",
          'data: {"type":"tool-input-start","toolCallId":"fc_perm_1","toolName":"PermissionConfirm"}',
          "",
          'data: {"type":"tool-input-available","toolCallId":"fc_perm_1","toolName":"PermissionConfirm","input":{"tool_name":"serverInfo","tool_args":{"scope":"name"}}}',
          "",
          'data: {"type":"tool-approval-request","approvalId":"fc_perm_1","toolCallId":"fc_perm_1"}',
          "",
          'data: {"type":"finish","finishReason":"tool-calls"}',
          "",
          "data: [DONE]",
          "",
        ].join("\n");

        await route.fulfill({
          status: 200,
          headers: {
            "content-type": "text/event-stream",
            "x-vercel-ai-ui-message-stream": "v1",
          },
          body: sse,
        });
        return;
      }

      resumeRequestBody = payload;
      const sse = [
        'data: {"type":"start","messageId":"m_perm_2"}',
        "",
        'data: {"type":"tool-input-start","toolCallId":"call_server_1","toolName":"serverInfo"}',
        "",
        'data: {"type":"tool-input-available","toolCallId":"call_server_1","toolName":"serverInfo","input":{"scope":"name"}}',
        "",
        'data: {"type":"tool-output-available","toolCallId":"call_server_1","output":{"data":{"name":"tirea-agentos"}}}',
        "",
        'data: {"type":"text-start","id":"txt_perm_2"}',
        "",
        'data: {"type":"text-delta","id":"txt_perm_2","delta":"Server is tirea-agentos."}',
        "",
        'data: {"type":"text-end","id":"txt_perm_2"}',
        "",
        'data: {"type":"finish","finishReason":"stop"}',
        "",
        "data: [DONE]",
        "",
      ].join("\n");

      await route.fulfill({
        status: 200,
        headers: {
          "content-type": "text/event-stream",
          "x-vercel-ai-ui-message-stream": "v1",
        },
        body: sse,
      });
    });

    await page.goto("/");

    await expect(page.getByTestId("page-title")).toContainText("AI SDK Framework Integration Playground", {
      timeout: 15_000,
    });

    const input = page.getByPlaceholder("Type a message...");
    await input.fill("Use the serverInfo tool to get server information.");
    await page.getByRole("button", { name: "Send" }).click();

    const dialog = page.getByTestId("permission-dialog");
    await expect(dialog).toBeVisible({ timeout: 45_000 });
    await expect(dialog).toContainText("serverInfo");

    const allowBtn = page.getByTestId("permission-allow");
    await expect(allowBtn).toBeVisible();
    await allowBtn.click();

    await expect.poll(() => chatRequestCount).toBeGreaterThanOrEqual(2);
    expect(resumeRequestBody).not.toBeNull();
    const resumeMessages = (resumeRequestBody?.messages as Array<Record<string, unknown>>) ?? [];
    const hasApprovalResponse = resumeMessages.some((message) => {
      const parts = (message.parts as Array<Record<string, unknown>>) ?? [];
      return parts.some((part) => {
        const approval = part.approval as { id?: string; approved?: boolean } | undefined;
        return (
          (part.type === "tool-approval-response" &&
            part.approvalId === "fc_perm_1" &&
            part.approved === true) ||
          (part.state === "approval-responded" &&
            approval?.id === "fc_perm_1" &&
            approval.approved === true)
        );
      });
    });
    expect(hasApprovalResponse).toBeTruthy();

    const chatArea = page.locator("main");
    await expect(chatArea).toContainText("Tool: serverInfo", { timeout: 45_000 });
    await expect(chatArea).toContainText("tirea-agentos", { timeout: 15_000 });
  });

  test("permission denial blocks backend tool execution", async ({ page }) => {
    await page.route("**/api/history?**", async (route) => {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({ messages: [] }),
      });
    });

    let chatRequestCount = 0;
    let resumeRequestBody: Record<string, unknown> | null = null;

    await page.route("**/api/chat", async (route) => {
      chatRequestCount += 1;
      const payloadText = route.request().postData() ?? "{}";
      const payload = JSON.parse(payloadText) as Record<string, unknown>;

      if (chatRequestCount === 1) {
        const sse = [
          'data: {"type":"start","messageId":"m_perm_deny_1"}',
          "",
          'data: {"type":"tool-input-start","toolCallId":"fc_perm_1","toolName":"PermissionConfirm"}',
          "",
          'data: {"type":"tool-input-available","toolCallId":"fc_perm_1","toolName":"PermissionConfirm","input":{"tool_name":"serverInfo","tool_args":{"scope":"name"}}}',
          "",
          'data: {"type":"tool-approval-request","approvalId":"fc_perm_1","toolCallId":"fc_perm_1"}',
          "",
          'data: {"type":"finish","finishReason":"tool-calls"}',
          "",
          "data: [DONE]",
          "",
        ].join("\n");

        await route.fulfill({
          status: 200,
          headers: {
            "content-type": "text/event-stream",
            "x-vercel-ai-ui-message-stream": "v1",
          },
          body: sse,
        });
        return;
      }

      resumeRequestBody = payload;
      const sse = [
        'data: {"type":"start","messageId":"m_perm_deny_2"}',
        "",
        'data: {"type":"tool-output-denied","toolCallId":"fc_perm_1"}',
        "",
        'data: {"type":"finish","finishReason":"stop"}',
        "",
        "data: [DONE]",
        "",
      ].join("\n");

      await route.fulfill({
        status: 200,
        headers: {
          "content-type": "text/event-stream",
          "x-vercel-ai-ui-message-stream": "v1",
        },
        body: sse,
      });
    });

    await page.goto("/");

    await expect(page.getByTestId("page-title")).toContainText("AI SDK Framework Integration Playground", {
      timeout: 15_000,
    });

    const input = page.getByPlaceholder("Type a message...");
    await input.fill("Use the serverInfo tool to get server information.");
    await page.getByRole("button", { name: "Send" }).click();

    const dialog = page.getByTestId("permission-dialog");
    await expect(dialog).toBeVisible({ timeout: 45_000 });
    await expect(dialog).toContainText("serverInfo");

    const denyBtn = page.getByTestId("permission-deny");
    await expect(denyBtn).toBeVisible();
    await denyBtn.click();

    await expect.poll(() => chatRequestCount).toBeGreaterThanOrEqual(2);
    expect(resumeRequestBody).not.toBeNull();
    const resumeMessages = (resumeRequestBody?.messages as Array<Record<string, unknown>>) ?? [];
    const hasApprovalResponse = resumeMessages.some((message) => {
      const parts = (message.parts as Array<Record<string, unknown>>) ?? [];
      return parts.some((part) => {
        const approval = part.approval as { id?: string; approved?: boolean } | undefined;
        return (
          (part.type === "tool-approval-response" &&
            part.approvalId === "fc_perm_1" &&
            part.approved === false) ||
          (part.state === "approval-responded" &&
            approval?.id === "fc_perm_1" &&
            approval.approved === false)
        );
      });
    });
    expect(hasApprovalResponse).toBeTruthy();

    const denied = page.getByTestId("permission-denied");
    await expect(denied).toBeVisible({ timeout: 30_000 });
    await expect(page.locator("main")).not.toContainText("tirea-agentos");
  });

  test("playground permission confirm approval resumes run", async ({ page }) => {
    await page.route("**/v1/threads?**", async (route) => {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({ items: ["thread-perm-approve"] }),
      });
    });
    await page.route("**/v1/ai-sdk/threads/**/messages?**", async (route) => {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({ messages: [] }),
      });
    });

    let chatRequestCount = 0;
    let resumeRequestBody: Record<string, unknown> | null = null;
    const chatHandler = async (route: Parameters<typeof page.route>[1] extends (
      ...args: infer P
    ) => unknown
      ? P[0]
      : never) => {
      chatRequestCount += 1;
      const payloadText = route.request().postData() ?? "{}";
      const payload = JSON.parse(payloadText) as Record<string, unknown>;

      if (chatRequestCount === 1) {
        const sse = [
          'data: {"type":"start","messageId":"m_perm_playground_1"}',
          "",
          'data: {"type":"tool-input-start","toolCallId":"perm_call_1","toolName":"PermissionConfirm"}',
          "",
          'data: {"type":"tool-input-available","toolCallId":"perm_call_1","toolName":"PermissionConfirm","input":{"tool_name":"serverInfo","tool_args":{"scope":"name"}}}',
          "",
          'data: {"type":"tool-approval-request","approvalId":"perm_call_1","toolCallId":"perm_call_1"}',
          "",
          'data: {"type":"finish","finishReason":"tool-calls"}',
          "",
          "data: [DONE]",
          "",
        ].join("\n");

        await route.fulfill({
          status: 200,
          headers: {
            "content-type": "text/event-stream",
            "x-vercel-ai-ui-message-stream": "v1",
          },
          body: sse,
        });
        return;
      }

      resumeRequestBody = payload;
      const sse = [
        'data: {"type":"start","messageId":"m_perm_playground_2"}',
        "",
        'data: {"type":"text-start","id":"txt_perm_playground_2"}',
        "",
        'data: {"type":"text-delta","id":"txt_perm_playground_2","delta":"Permission granted. Server is tirea-agentos."}',
        "",
        'data: {"type":"text-end","id":"txt_perm_playground_2"}',
        "",
        'data: {"type":"finish","finishReason":"stop"}',
        "",
        "data: [DONE]",
        "",
      ].join("\n");

      await route.fulfill({
        status: 200,
        headers: {
          "content-type": "text/event-stream",
          "x-vercel-ai-ui-message-stream": "v1",
        },
        body: sse,
      });
    };
    await page.route("**/v1/ai-sdk/agents/**/runs", chatHandler);
    await page.route("**/api/chat", chatHandler);

    await page.goto("/");

    await expect(page.getByTestId("page-title")).toContainText(
      "AI SDK Framework Integration Playground",
      { timeout: 15_000 },
    );
    await expect(page.getByTestId("active-thread")).toContainText(
      "thread-perm-approve",
    );

    await page
      .getByPlaceholder("Type a message...")
      .fill("Use serverInfo tool and ask for permission.");
    await page.getByRole("button", { name: "Send" }).click();

    const dialog = page.getByTestId("permission-dialog");
    await expect(dialog).toBeVisible({ timeout: 30_000 });
    await expect(dialog).toContainText("serverInfo");
    await page.getByTestId("permission-allow").click();

    await expect(page.locator("main")).toContainText(
      "Permission granted. Server is tirea-agentos.",
      { timeout: 30_000 },
    );
    await expect.poll(() => chatRequestCount).toBeGreaterThanOrEqual(2);

    const resumeMessages = (resumeRequestBody?.messages as Array<Record<string, unknown>>) ?? [];
    const hasApprovalResponse = resumeMessages.some((message) => {
      const parts = (message.parts as Array<Record<string, unknown>>) ?? [];
      return parts.some((part) => {
        const approval = part.approval as { id?: string; approved?: boolean } | undefined;
        return (
          (part.type === "tool-approval-response" &&
            part.approvalId === "perm_call_1" &&
            part.approved === true) ||
          (part.state === "approval-responded" &&
            approval?.id === "perm_call_1" &&
            approval.approved === true)
        );
      });
    });
    expect(hasApprovalResponse).toBeTruthy();
  });

  test("playground permission confirm denial keeps tool blocked", async ({ page }) => {
    await page.route("**/v1/threads?**", async (route) => {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({ items: ["thread-perm-deny"] }),
      });
    });
    await page.route("**/v1/ai-sdk/threads/**/messages?**", async (route) => {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({ messages: [] }),
      });
    });

    let chatRequestCount = 0;
    let resumeRequestBody: Record<string, unknown> | null = null;
    const chatHandler = async (route: Parameters<typeof page.route>[1] extends (
      ...args: infer P
    ) => unknown
      ? P[0]
      : never) => {
      chatRequestCount += 1;
      const payloadText = route.request().postData() ?? "{}";
      const payload = JSON.parse(payloadText) as Record<string, unknown>;

      if (chatRequestCount === 1) {
        const sse = [
          'data: {"type":"start","messageId":"m_perm_playground_deny_1"}',
          "",
          'data: {"type":"tool-input-start","toolCallId":"perm_call_deny_1","toolName":"PermissionConfirm"}',
          "",
          'data: {"type":"tool-input-available","toolCallId":"perm_call_deny_1","toolName":"PermissionConfirm","input":{"tool_name":"serverInfo","tool_args":{"scope":"name"}}}',
          "",
          'data: {"type":"tool-approval-request","approvalId":"perm_call_deny_1","toolCallId":"perm_call_deny_1"}',
          "",
          'data: {"type":"finish","finishReason":"tool-calls"}',
          "",
          "data: [DONE]",
          "",
        ].join("\n");

        await route.fulfill({
          status: 200,
          headers: {
            "content-type": "text/event-stream",
            "x-vercel-ai-ui-message-stream": "v1",
          },
          body: sse,
        });
        return;
      }

      resumeRequestBody = payload;
      const sse = [
        'data: {"type":"start","messageId":"m_perm_playground_deny_2"}',
        "",
        'data: {"type":"tool-output-denied","toolCallId":"perm_call_deny_1"}',
        "",
        'data: {"type":"text-start","id":"txt_perm_playground_deny_2"}',
        "",
        'data: {"type":"text-delta","id":"txt_perm_playground_deny_2","delta":"Permission denied by user."}',
        "",
        'data: {"type":"text-end","id":"txt_perm_playground_deny_2"}',
        "",
        'data: {"type":"finish","finishReason":"stop"}',
        "",
        "data: [DONE]",
        "",
      ].join("\n");

      await route.fulfill({
        status: 200,
        headers: {
          "content-type": "text/event-stream",
          "x-vercel-ai-ui-message-stream": "v1",
        },
        body: sse,
      });
    };
    await page.route("**/v1/ai-sdk/agents/**/runs", chatHandler);
    await page.route("**/api/chat", chatHandler);

    await page.goto("/");

    await expect(page.getByTestId("page-title")).toContainText(
      "AI SDK Framework Integration Playground",
      { timeout: 15_000 },
    );

    await page
      .getByPlaceholder("Type a message...")
      .fill("Use serverInfo tool and ask for permission.");
    await page.getByRole("button", { name: "Send" }).click();

    const dialog = page.getByTestId("permission-dialog");
    await expect(dialog).toBeVisible({ timeout: 30_000 });
    await page.getByTestId("permission-deny").click();

    await expect(page.locator("main")).toContainText("Permission denied by user.", {
      timeout: 30_000,
    });
    await expect(page.getByTestId("permission-denied")).toBeVisible({
      timeout: 30_000,
    });
    await expect.poll(() => chatRequestCount).toBeGreaterThanOrEqual(2);

    const resumeMessages = (resumeRequestBody?.messages as Array<Record<string, unknown>>) ?? [];
    const hasApprovalResponse = resumeMessages.some((message) => {
      const parts = (message.parts as Array<Record<string, unknown>>) ?? [];
      return parts.some((part) => {
        const approval = part.approval as { id?: string; approved?: boolean } | undefined;
        return (
          (part.type === "tool-approval-response" &&
            part.approvalId === "perm_call_deny_1" &&
            part.approved === false) ||
          (part.state === "approval-responded" &&
            approval?.id === "perm_call_deny_1" &&
            approval.approved === false)
        );
      });
    });
    expect(hasApprovalResponse).toBeTruthy();
  });

  test("askUserQuestion submits frontend answer and resumes run", async ({ page }) => {
    await page.route("**/api/history?**", async (route) => {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({ messages: [] }),
      });
    });

    let chatRequestCount = 0;
    let resumeRequestBody: Record<string, unknown> | null = null;

    await page.route("**/api/chat", async (route) => {
      chatRequestCount += 1;
      const payloadText = route.request().postData() ?? "{}";
      const payload = JSON.parse(payloadText) as Record<string, unknown>;

      if (chatRequestCount === 1) {
        const sse = [
          'data: {"type":"start","messageId":"m_ask_1"}',
          "",
          'data: {"type":"tool-input-start","toolCallId":"ask_call_1","toolName":"askUserQuestion"}',
          "",
          'data: {"type":"tool-input-available","toolCallId":"ask_call_1","toolName":"askUserQuestion","input":{"message":"What is your favorite color?"}}',
          "",
          'data: {"type":"finish","finishReason":"tool-calls"}',
          "",
          "data: [DONE]",
          "",
        ].join("\n");

        await route.fulfill({
          status: 200,
          headers: {
            "content-type": "text/event-stream",
            "x-vercel-ai-ui-message-stream": "v1",
          },
          body: sse,
        });
        return;
      }

      resumeRequestBody = payload;

      const sse = [
        'data: {"type":"start","messageId":"m_ask_2"}',
        "",
        'data: {"type":"text-start","id":"txt_ask_2"}',
        "",
        'data: {"type":"text-delta","id":"txt_ask_2","delta":"You said blue."}',
        "",
        'data: {"type":"text-end","id":"txt_ask_2"}',
        "",
        'data: {"type":"finish","finishReason":"stop"}',
        "",
        "data: [DONE]",
        "",
      ].join("\n");

      await route.fulfill({
        status: 200,
        headers: {
          "content-type": "text/event-stream",
          "x-vercel-ai-ui-message-stream": "v1",
        },
        body: sse,
      });
    });

    await page.goto("/");

    await expect(page.getByTestId("page-title")).toContainText("AI SDK Framework Integration Playground", {
      timeout: 15_000,
    });

    await page.getByPlaceholder("Type a message...").fill("Please ask me a question.");
    await page.getByRole("button", { name: "Send" }).click();

    const askDialog = page.getByTestId("ask-dialog");
    await expect(askDialog).toBeVisible({ timeout: 20_000 });
    await expect(page.getByTestId("ask-question-prompt")).toContainText("favorite color");

    await page.getByTestId("ask-question-input").fill("blue");
    await page.getByTestId("ask-question-submit").click();

    await expect(page.locator("main")).toContainText("You said blue.", {
      timeout: 20_000,
    });

    await expect.poll(() => chatRequestCount).toBeGreaterThanOrEqual(2);
    expect(resumeRequestBody).not.toBeNull();

    const resumeMessages = (resumeRequestBody?.messages as Array<Record<string, unknown>>) ?? [];
    const hasAskOutput = resumeMessages.some((message) => {
      const parts = (message.parts as Array<Record<string, unknown>>) ?? [];
      return parts.some(
        (part) =>
          (part.type === "tool-askUserQuestion" || part.type === "dynamic-tool") &&
          part.state === "output-available" &&
          (part.output as { message?: string } | undefined)?.message === "blue"
      );
    });
    expect(hasAskOutput).toBeTruthy();
  });

  test("frontend set_background_color tool updates surface and resumes run", async ({
    page,
  }) => {
    await page.route("**/api/history?**", async (route) => {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({ messages: [] }),
      });
    });

    let chatRequestCount = 0;
    let resumeRequestBody: Record<string, unknown> | null = null;

    await page.route("**/api/chat", async (route) => {
      chatRequestCount += 1;
      const payloadText = route.request().postData() ?? "{}";
      const payload = JSON.parse(payloadText) as Record<string, unknown>;

      if (chatRequestCount === 1) {
        const sse = [
          'data: {"type":"start","messageId":"m_color_1"}',
          "",
          'data: {"type":"tool-input-start","toolCallId":"bg_call_1","toolName":"set_background_color"}',
          "",
          'data: {"type":"tool-input-available","toolCallId":"bg_call_1","toolName":"set_background_color","input":{"colors":["#dbeafe","#dcfce7"]}}',
          "",
          'data: {"type":"finish","finishReason":"tool-calls"}',
          "",
          "data: [DONE]",
          "",
        ].join("\n");

        await route.fulfill({
          status: 200,
          headers: {
            "content-type": "text/event-stream",
            "x-vercel-ai-ui-message-stream": "v1",
          },
          body: sse,
        });
        return;
      }

      resumeRequestBody = payload;
      const sse = [
        'data: {"type":"start","messageId":"m_color_2"}',
        "",
        'data: {"type":"text-start","id":"txt_color_2"}',
        "",
        'data: {"type":"text-delta","id":"txt_color_2","delta":"Background color updated to #dbeafe."}',
        "",
        'data: {"type":"text-end","id":"txt_color_2"}',
        "",
        'data: {"type":"finish","finishReason":"stop"}',
        "",
        "data: [DONE]",
        "",
      ].join("\n");

      await route.fulfill({
        status: 200,
        headers: {
          "content-type": "text/event-stream",
          "x-vercel-ai-ui-message-stream": "v1",
        },
        body: sse,
      });
    });

    await page.goto("/");

    await page
      .getByPlaceholder("Type a message...")
      .fill("Please change the chat background color.");
    await page.getByRole("button", { name: "Send" }).click();

    const frontendDialog = page.getByTestId("set-background-color-dialog");
    await expect(frontendDialog).toBeVisible({ timeout: 20_000 });
    const colorOption = page.getByTestId("set-background-color-option-0");
    await expect(colorOption).toBeVisible();
    await colorOption.click();

    await expect(page.locator("main")).toContainText(
      "Background color updated to #dbeafe.",
      { timeout: 20_000 },
    );
    await expect(page.getByTestId("chat-frontend-surface")).toHaveAttribute(
      "style",
      /#dbeafe22/i,
    );

    await expect.poll(() => chatRequestCount).toBeGreaterThanOrEqual(2);
    expect(resumeRequestBody).not.toBeNull();

    const resumeMessages = (resumeRequestBody?.messages as Array<Record<string, unknown>>) ?? [];
    const hasFrontendToolOutput = resumeMessages.some((message) => {
      const parts = (message.parts as Array<Record<string, unknown>>) ?? [];
      return parts.some((part) => {
        if (
          !(
            (part.type === "tool-set_background_color" ||
              part.type === "dynamic-tool") &&
            part.state === "output-available"
          )
        ) {
          return false;
        }
        const output = part.output as { color?: string } | undefined;
        return output?.color === "#dbeafe";
      });
    });
    expect(hasFrontendToolOutput).toBeTruthy();
  });

  test("StopOnTool terminates agent run", async ({ page }) => {
    await page.goto("/?agentId=stopper");

    await expect(page.getByTestId("page-title")).toContainText("AI SDK Framework Integration Playground", {
      timeout: 15_000,
    });

    const input = page.getByPlaceholder("Type a message...");
    await input.fill("What is 2+2?");
    await page.getByRole("button", { name: "Send" }).click();

    // Wait for streaming to finish.
    const sendButton = page.getByRole("button", { name: "Send" });
    await expect(sendButton).toBeEnabled({ timeout: 30_000 });

    const chatArea = page.locator("main");

    // Agent should answer with "4".
    await expect(chatArea).toContainText("4", { timeout: 10_000 });

    // The finish tool should have been called (tool display rendered).
    await expect(chatArea).toContainText("Tool: finish", {
      timeout: 10_000,
    });

    // Run terminated — send button is enabled again.
    await expect(sendButton).toBeEnabled();
  });

  test("send button is disabled while loading", async ({ page }) => {
    await page.goto("/");

    await expect(page.getByTestId("page-title")).toContainText("AI SDK Framework Integration Playground", {
      timeout: 15_000,
    });

    const input = page.getByPlaceholder("Type a message...");
    const sendButton = page.getByRole("button", { name: "Send" });

    await input.fill("Tell me a short joke.");
    await sendButton.click();

    // The button should be disabled while waiting.
    await expect(sendButton).toBeDisabled({ timeout: 2_000 });

    // Wait for completion, then button should be enabled again.
    await expect(sendButton).toBeEnabled({ timeout: 30_000 });
  });

  test("renders reasoning, sources, files and tool-invocation from history payload", async ({
    page,
  }) => {
    await page.route("**/api/history?**", async (route) => {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          messages: [
            {
              id: "u1",
              role: "user",
              parts: [{ type: "text", text: "show me details" }],
            },
            {
              id: "a1",
              role: "assistant",
              parts: [
                { type: "reasoning", text: "internal chain", state: "done" },
                {
                  type: "source-url",
                  sourceId: "s1",
                  url: "https://example.com",
                  title: "Example Source",
                },
                {
                  type: "source-document",
                  sourceId: "d1",
                  mediaType: "application/pdf",
                  title: "Spec",
                  filename: "spec.pdf",
                },
                {
                  type: "file",
                  url: "https://example.com/report.csv",
                  mediaType: "text/csv",
                },
                {
                  type: "tool-invocation",
                  toolCallId: "call_1",
                  toolName: "serverInfo",
                  state: "output-available",
                  output: { name: "tirea-agentos" },
                },
              ],
            },
          ],
        }),
      });
    });

    await page.goto("/");
    await expect(page.getByTestId("page-title")).toContainText("AI SDK Framework Integration Playground", {
      timeout: 15_000,
    });

    await expect(page.getByTestId("reasoning-part")).toBeVisible();
    await expect(page.getByTestId("source-url-part")).toContainText("Example Source");
    await expect(page.getByTestId("source-document-part")).toContainText("spec.pdf");
    await expect(page.getByTestId("file-part")).toContainText("report.csv");
    await expect(page.locator("main")).toContainText("Tool: serverInfo");
  });

  test("parses streamed reasoning/source/file/tool events from ai-sdk protocol", async ({
    page,
  }) => {
    await page.route("**/api/history?**", async (route) => {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({ messages: [] }),
      });
    });

    await page.route("**/api/chat", async (route) => {
      const sse = [
        'data: {"type":"start","messageId":"m_stream"}',
        "",
        'data: {"type":"reasoning-start","id":"r_1"}',
        "",
        'data: {"type":"reasoning-delta","id":"r_1","delta":"thinking"}',
        "",
        'data: {"type":"reasoning-end","id":"r_1"}',
        "",
        'data: {"type":"source-url","sourceId":"s_1","url":"https://example.com","title":"Example"}',
        "",
        'data: {"type":"source-document","sourceId":"d_1","mediaType":"application/pdf","title":"Spec","filename":"spec.pdf"}',
        "",
        'data: {"type":"file","url":"https://example.com/report.csv","mediaType":"text/csv"}',
        "",
        'data: {"type":"tool-input-start","toolCallId":"call_1","toolName":"serverInfo"}',
        "",
        'data: {"type":"tool-input-available","toolCallId":"call_1","toolName":"serverInfo","input":{"scope":"name"}}',
        "",
        'data: {"type":"tool-output-available","toolCallId":"call_1","output":{"name":"tirea-agentos"}}',
        "",
        'data: {"type":"text-start","id":"txt_1"}',
        "",
        'data: {"type":"text-delta","id":"txt_1","delta":"Server is tirea-agentos."}',
        "",
        'data: {"type":"text-end","id":"txt_1"}',
        "",
        'data: {"type":"data-inference-complete","data":{"model":"deepseek-chat","usage":{"prompt_tokens":10,"completion_tokens":5},"duration_ms":123}}',
        "",
        'data: {"type":"finish","finishReason":"stop"}',
        "",
        "data: [DONE]",
        "",
      ].join("\n");

      await route.fulfill({
        status: 200,
        headers: {
          "content-type": "text/event-stream",
          "x-vercel-ai-ui-message-stream": "v1",
        },
        body: sse,
      });
    });

    await page.goto("/");
    await expect(page.getByTestId("page-title")).toContainText("AI SDK Framework Integration Playground", {
      timeout: 15_000,
    });

    await page.getByPlaceholder("Type a message...").fill("show rich events");
    await page.getByRole("button", { name: "Send" }).click();

    await expect(page.locator("main")).toContainText("Server is tirea-agentos.");
    await expect(page.getByTestId("reasoning-part")).toBeVisible();
    await expect(page.getByTestId("source-url-part")).toContainText("Example");
    await expect(page.getByTestId("source-document-part")).toContainText("spec.pdf");
    await expect(page.getByTestId("file-part")).toContainText("report.csv");
    await expect(page.locator("main")).toContainText("Tool: serverInfo");
    await expect(page.getByTestId("metrics-panel")).toBeVisible();
  });

  test("handles ai-sdk abort event and keeps partial output", async ({ page }) => {
    await page.route("**/api/history?**", async (route) => {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({ messages: [] }),
      });
    });

    await page.route("**/api/chat", async (route) => {
      const sse = [
        'data: {"type":"start","messageId":"m_abort"}',
        "",
        'data: {"type":"text-start","id":"txt_abort"}',
        "",
        'data: {"type":"text-delta","id":"txt_abort","delta":"partial answer"}',
        "",
        'data: {"type":"text-end","id":"txt_abort"}',
        "",
        'data: {"type":"abort","reason":"cancelled"}',
        "",
        "data: [DONE]",
        "",
      ].join("\n");

      await route.fulfill({
        status: 200,
        headers: {
          "content-type": "text/event-stream",
          "x-vercel-ai-ui-message-stream": "v1",
        },
        body: sse,
      });
    });

    await page.goto("/");
    await expect(page.getByTestId("page-title")).toContainText("AI SDK Framework Integration Playground", {
      timeout: 15_000,
    });

    const input = page.getByPlaceholder("Type a message...");
    const sendButton = page.getByRole("button", { name: "Send" });

    await input.fill("trigger abort");
    await sendButton.click();

    await expect(page.locator("main")).toContainText("partial answer");
    await expect(sendButton).toBeEnabled({ timeout: 10_000 });
    await expect(page.locator("main")).not.toContainText("Error:");
  });

  test("surfaces ai-sdk error event in UI", async ({ page }) => {
    await page.route("**/api/history?**", async (route) => {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({ messages: [] }),
      });
    });

    await page.route("**/api/chat", async (route) => {
      const sse = [
        'data: {"type":"start","messageId":"m_error"}',
        "",
        'data: {"type":"error","errorText":"upstream failure"}',
        "",
        "data: [DONE]",
        "",
      ].join("\n");

      await route.fulfill({
        status: 200,
        headers: {
          "content-type": "text/event-stream",
          "x-vercel-ai-ui-message-stream": "v1",
        },
        body: sse,
      });
    });

    await page.goto("/");
    await expect(page.getByTestId("page-title")).toContainText("AI SDK Framework Integration Playground", {
      timeout: 15_000,
    });

    await page.getByPlaceholder("Type a message...").fill("trigger error");
    await page.getByRole("button", { name: "Send" }).click();

    await expect(page.locator("main")).toContainText("Error: upstream failure", {
      timeout: 10_000,
    });
  });
});
