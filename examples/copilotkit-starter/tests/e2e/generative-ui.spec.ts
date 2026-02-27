import { expect, test } from "@playwright/test";

test("renders release checklist items for showReleaseChecklist prompt", async ({ page }) => {
  await page.goto("/v1");

  await page.getByTestId("generative-ui-local-preview-trigger").click();

  const checklist = page.getByTestId("generative-ui-local-checklist");
  await expect(checklist).toBeVisible();

  const checklistText = (await checklist.textContent()) ?? "";
  expect(checklistText).toContain("run tests");
  expect(checklistText).toContain("tag release");

  await expect(page.getByTestId("generative-ui-card-error")).toHaveCount(0);
  await expect(page.getByTestId("generative-ui-local-preview-clear")).toBeEnabled();
});
