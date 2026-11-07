// Windows mode with eight sandboxes on a 390 px phone (playwright.windows8.config.ts): the
// demonstration label is whole on every tile, its text inside its own box and the box inside
// its caption, with the take-over panel closed and open.

import { expect, test, type Page } from "@playwright/test";
import { labelIsWhole } from "./helpers.ts";

const SLOTS = Array.from({ length: 8 }, (_, i) => `win-${i + 1}`);
const LABEL = "Windows Server 2025 evaluation, for demonstration only";

async function everyLabelIsWhole(page: Page): Promise<void> {
  for (const slot of SLOTS) {
    await expect(page.getByTestId(`sandbox-label-${slot}`)).toHaveText(LABEL);
    expect(await labelIsWhole(page, slot), `${slot}'s label overflows`).toEqual({ fits: true, inside: true });
  }
}

test("eight sandboxes on a 390 px phone: every label is whole, with the panel closed and open", async ({ page }) => {
  await page.goto("/?mode=windows&debug=1");
  await page.waitForFunction(() => window.__windows !== undefined);
  await expect.poll(async () => page.evaluate(() => window.__windows!.view().cards.filter((c) => c.box !== null).length), { timeout: 30_000 }).toBe(8);
  await everyLabelIsWhole(page);
  await expect(page.getByTestId("sandbox-take-win-2")).toBeEnabled({ timeout: 15_000 });
  await page.getByTestId("sandbox-take-win-2").click();
  await expect(page.getByTestId("win-panel")).toBeVisible();
  await everyLabelIsWhole(page);
  await page.getByTestId("win-give-back").click();
  await expect(page.getByTestId("win-panel")).toHaveCount(0);
});
