import { expect, test } from "@playwright/test";
import summary from "../app/boot-race/summary.json" with { type: "json" };

// /boot-race: recorded campaign results, linked from the wall's About panel. It must say it is recorded, name what
// was not run, and fit a phone without scrolling sideways.

test("the wall's About panel links to the recorded boot race", async ({ page }) => {
  await page.goto("/?debug=1");
  await page.getByTestId("menu").first().click();
  const link = page.getByTestId("boot-race-link");
  await expect(link).toHaveAttribute("href", "/boot-race");
  await link.click();
  await expect(page).toHaveURL(/\/boot-race$/);
  await expect(page.getByTestId("boot-race-recorded")).toContainText("Recorded results, not live.");
});

for (const width of [360, 1024]) {
  test(`the boot race page states its campaign and fits ${width} px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 780 });
    await page.goto("/boot-race");
    const recorded = page.getByTestId("boot-race-recorded");
    await expect(recorded).toContainText(summary.campaign);
    await expect(page.getByText(summary.fcvm_commit)).toBeVisible();
    await expect(page.getByRole("link", { name: new RegExp(summary.records.repository) })).toHaveAttribute("href", summary.records.url);
    for (const lane of summary.lanes) {
      const card = page.getByTestId(`boot-race-lane-${lane.id}`);
      await expect(card.getByRole("heading", { name: lane.name, exact: true })).toBeVisible();
      await expect(card).toContainText(lane.config);
    }
    for (const lane of summary.not_run) await expect(page.getByTestId(`boot-race-notrun-${lane.id}`)).toContainText(`Not run: ${lane.name}.`);
    // Nothing is wider than the page's scroll box: no sideways scroll at any width.
    const over = await page.getByTestId("boot-race").evaluate((el) => el.scrollWidth - el.clientWidth);
    expect(over).toBeLessThanOrEqual(0);
    const doc = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
    expect(doc).toBeLessThanOrEqual(0);
  });
}
