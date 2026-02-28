import { expect, test } from "@playwright/test";

test("renders MCP app iframe via local preview", async ({ page }) => {
  await page.goto("/v1");

  await page.getByTestId("mcp-app-local-preview-trigger").click();

  const frame = page.getByTestId("mcp-app-frame-iframe");
  await expect(frame).toBeVisible();
  await expect(frame).toHaveAttribute("sandbox", "allow-scripts");

  const iframeContent = page.frameLocator('[data-testid="mcp-app-frame-iframe"]');
  await expect(iframeContent.locator("body")).toContainText("MCP App Preview");
  await expect(iframeContent.locator("body")).toContainText("sandboxed MCP application");

  const resourceLabel = page.locator('[data-testid="mcp-app-frame"] .text-slate-400');
  await expect(resourceLabel).toContainText("ui://demo/preview");
});

test("clear button removes MCP app preview", async ({ page }) => {
  await page.goto("/v1");

  const clearButton = page.getByTestId("mcp-app-local-preview-clear");
  await expect(clearButton).toBeDisabled();

  await page.getByTestId("mcp-app-local-preview-trigger").click();
  await expect(page.getByTestId("mcp-app-frame-iframe")).toBeVisible();
  await expect(clearButton).toBeEnabled();

  await clearButton.click();
  await expect(page.getByTestId("mcp-app-frame-iframe")).toHaveCount(0);
  await expect(clearButton).toBeDisabled();
});

test("MCP app iframe does not appear without triggering preview", async ({ page }) => {
  await page.goto("/v1");

  await expect(page.getByTestId("mcp-app-frame-iframe")).toHaveCount(0);
  await expect(page.getByTestId("mcp-app-frame")).toHaveCount(0);
});
