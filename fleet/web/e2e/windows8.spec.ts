// Windows mode with eight sandboxes on a 390 px phone (playwright.windows8.config.ts): every
// caption is whole (its slot, time to desktop and Take over inside it), and the demonstration
// label is whole once in the header, with the take-over panel closed and open.

import { expect, test, type Page } from "@playwright/test";
import { wholeWithin } from "./helpers.ts";

const SLOTS = Array.from({ length: 8 }, (_, i) => `win-${i + 1}`);
const LABEL = "Windows Server 2025 evaluation, for demonstration only";

async function everyCaptionIsWhole(page: Page, take: boolean): Promise<void> {
  for (const slot of SLOTS) {
    const caption = `sandbox-caption-${slot}`;
    expect(await wholeWithin(page, `sandbox-slot-${slot}`, caption), `${slot}'s slot`).toEqual({ fits: true, inside: true });
    expect(await wholeWithin(page, `sandbox-desktop-in-${slot}`, caption), `${slot}'s time to desktop`).toEqual({ fits: true, inside: true });
    if (take) expect(await wholeWithin(page, `sandbox-take-${slot}`, caption), `${slot}'s Take over`).toEqual({ fits: true, inside: true });
  }
  await expect(page.getByTestId("win-label")).toHaveText(LABEL);
  await expect(page.getByTestId("win-label")).toBeInViewport({ ratio: 1 });
}

test("eight sandboxes on a 390 px phone: every caption and the label are whole, with the panel closed and open", async ({ page }) => {
  await page.goto("/?mode=windows&debug=1");
  await page.waitForFunction(() => window.__windows !== undefined);
  await expect.poll(async () => page.evaluate(() => window.__windows!.view().cards.filter((c) => c.box !== null).length), { timeout: 30_000 }).toBe(8);
  await expect(page.getByTestId("sandbox-desktop-in-win-8")).toBeVisible({ timeout: 15_000 });
  await everyCaptionIsWhole(page, true);
  await expect(page.getByTestId("sandbox-take-win-2")).toBeEnabled({ timeout: 15_000 });
  await page.getByTestId("sandbox-take-win-2").click();
  await expect(page.getByTestId("win-panel")).toBeVisible();
  // With the panel open a compact caption leaves Take over out (showTake).
  await everyCaptionIsWhole(page, false);
  await page.getByTestId("win-give-back").click();
  await expect(page.getByTestId("win-panel")).toHaveCount(0);
});
