import { expect, test } from "@playwright/test";

test("Webhook setup closes the outbound notification lifecycle", async ({ page }) => {
  const endpoint = `https://events-${Date.now()}.example.com/awaken`;
  await page.goto("/w/default/webhooks");

  await expect(page.getByRole("heading", { name: "Webhooks", exact: true })).toBeVisible();
  await expect(page.getByText("they are not an inbound protocol for calling an Agent", { exact: false })).toBeVisible();

  await page.getByLabel("Public HTTPS URL").fill(endpoint);
  await page.getByLabel("Event types · optional").fill(
    "session.status_idled\nsession.status_terminated\nsession.status_idled",
  );
  await page.getByRole("button", { name: "Create endpoint" }).click();

  const oneTimeSecret = page.locator(".webhook-secret-copy code");
  await expect(page.getByText("Signing secret shown once", { exact: true })).toBeVisible();
  await expect(oneTimeSecret).toHaveText(/^whsec_/);

  const row = page.locator("tbody tr", { hasText: endpoint });
  await expect(row).toContainText("session.status_idled");
  await expect(row).toContainText("session.status_terminated");
  await expect(row.locator("code", { hasText: "session.status_idled" })).toHaveCount(1);
  await expect(row).toContainText("Active");

  await row.getByRole("button", { name: "Pause" }).click();
  await expect(row).toContainText("Paused");
  await row.getByRole("button", { name: "Enable" }).click();
  await expect(row).toContainText("Active");

  await page.setViewportSize({ width: 390, height: 844 });
  await expect(row).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);

  await page.reload();
  await expect(page.getByText("Signing secret shown once", { exact: true })).toHaveCount(0);
  const reloadedRow = page.locator("tbody tr", { hasText: endpoint });
  await expect(reloadedRow).toBeVisible();
  await reloadedRow.getByRole("button", { name: "Delete" }).click();
  await page.getByRole("button", { name: "Delete webhook" }).click();
  await expect(page.getByText("No webhook endpoints", { exact: true })).toBeVisible();
});
