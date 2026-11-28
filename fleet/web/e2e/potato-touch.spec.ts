// The potato page on a 360 px touch screen (the "phone" project of playwright.potato.config.ts):
// a long press shows a tile's readout wholly inside the stage, a plain tap does not.

import { expect, test, type Page } from "@playwright/test";
import { admin, mockState } from "./helpers.ts";

async function open(page: Page): Promise<void> {
  await admin("/mock/potato", { cpus: 0, players: 8, size: 8, next_in_ms: 500 });
  await expect.poll(async () => (((await mockState()).state as unknown as { potato: { round: unknown } }).potato.round ? 1 : 0), { timeout: 10_000 }).toBe(1);
  await page.goto("/?mode=potato&debug=1");
  await page.waitForFunction(() => window.__potato !== undefined);
}

const touch = (page: Page, index: number, type: "pointerdown" | "pointerup") =>
  page.getByTestId("potato-tile").nth(index).evaluate(
    (el, t) => el.dispatchEvent(new PointerEvent(t, { pointerType: "touch", bubbles: true, isPrimary: true, pointerId: 7 })),
    type,
  );

test("a long press on a tile on each side of the ring shows its readout inside the stage; a tap shows none", async ({ page }) => {
  await open(page);
  const stage = (await page.getByTestId("potato-stage").boundingBox())!;
  const readout = page.getByTestId("potato-readout");
  await page.getByTestId("potato-tile").nth(0).tap();
  await page.waitForTimeout(500);
  await expect(readout, "a plain tap shows nothing").toHaveCount(0);
  // 8 players: 0 top, 2 right, 4 bottom, 6 left
  for (const i of [0, 2, 4, 6, 1, 7]) {
    await touch(page, i, "pointerdown");
    await expect(readout).toBeVisible({ timeout: 3000 });
    const r = (await readout.boundingBox())!;
    const t = (await page.getByTestId("potato-tile").nth(i).boundingBox())!;
    expect(r.x, `tile ${i} left`).toBeGreaterThanOrEqual(stage.x - 1);
    expect(r.y, `tile ${i} top`).toBeGreaterThanOrEqual(stage.y - 1);
    expect(r.x + r.width, `tile ${i} right`).toBeLessThanOrEqual(stage.x + stage.width + 1);
    expect(r.y + r.height, `tile ${i} bottom`).toBeLessThanOrEqual(stage.y + stage.height + 1);
    expect(r.x + r.width <= t.x + 1 || t.x + t.width <= r.x + 1 || r.y + r.height <= t.y + 1 || t.y + t.height <= r.y + 1, `tile ${i} clear of its card`).toBe(true);
    await touch(page, i, "pointerup");
    await expect(readout).toHaveCount(0);
  }
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);
});
