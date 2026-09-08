import { expect, test } from "@playwright/test";

// Agents integration table: legacy explicit preference -> prepaint migration;
// refresh -> same canonical preference; selecting system -> OS updates remain live.
// Pointer density comes from the family owner at every viewport.
// Chinese + narrow chrome -> visible operable appearance control with bounded layout.
test("Agents migrates appearance and keeps the Chinese mobile chrome usable", async ({ page }) => {
  await page.addInitScript(() => {
    if (!localStorage.getItem("appearance-fixture-initialized")) {
      localStorage.setItem("awaken.console.theme", "dark");
      localStorage.setItem("awaken.console.locale", "zh");
      localStorage.setItem("appearance-fixture-initialized", "true");
    }
  });
  await page.emulateMedia({ colorScheme: "light" });
  await page.goto("/overview");
  const appearance = page.getByRole("combobox", { name: "外观" });
  await expect(appearance).toHaveValue("dark");
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
  expect(await page.evaluate(() => localStorage.getItem("awaken.console.theme"))).toBeNull();
  await appearance.selectOption("light");
  await page.reload();
  await expect(appearance).toHaveValue("light");
  await appearance.selectOption("system");
  await page.emulateMedia({ colorScheme: "dark" });
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
  await page.setViewportSize({ width: 360, height: 780 });
  await expect(appearance).toBeVisible();
  expect(await appearance.evaluate((node) => getComputedStyle(node).getPropertyValue("--ui-control-height-md").trim()))
    .toBe(await page.evaluate(() => matchMedia("(pointer: coarse)").matches ? "44px" : "34px"));
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.screenshot({ path: "test-results/agents-appearance-mobile-zh.png", fullPage: true });
});

// Storage denial must not crash another presentation owner before the theme
// selector mounts. Explicit theme and language changes remain usable in memory.
test("Agents appearance remains usable when browser storage is blocked", async ({ page }) => {
  await page.addInitScript(() => {
    Object.defineProperty(window, "localStorage", { get() { throw new DOMException("blocked", "SecurityError"); } });
  });
  await page.emulateMedia({ colorScheme: "dark" });
  await page.goto("/overview");
  const appearance = page.getByRole("combobox", { name: "Appearance" });
  await expect(appearance).toHaveValue("system");
  await appearance.selectOption("light");
  await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
  await page.getByRole("button", { name: "Switch language" }).click();
  await expect(page.getByRole("combobox", { name: "外观" })).toHaveValue("light");
});
