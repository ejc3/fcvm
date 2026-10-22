// The pictures for the PR and the README: the wall at rest with viewers' words
// and lights, hover, the stats switch, mid-race, an opened tile and a phone.
// Written to FLEET_SCREENSHOT_DIR when set, else to the test's output folder.

import { expect, test, devices, type Page } from "@playwright/test";
import { openWall } from "./helpers.ts";

async function shot(page: Page, name: string): Promise<void> {
  const dir = process.env.FLEET_SCREENSHOT_DIR ?? test.info().outputPath();
  await page.screenshot({ path: `${dir}/${name}.jpg`, type: "jpeg", quality: 86 });
}

const tileBox = (page: Page, name: string) =>
  page.evaluate((n) => window.__fleet!.view().tiles.find((t) => t.name === n)!.box!, name);

test("screenshots", async ({ page, browser }) => {
  test.setTimeout(120_000);
  await openWall(page, 15);
  // Give the scene some viewers: words, lights, an evening sun.
  for (const text of ["hello fleet", "fcvm"]) {
    await page.getByLabel("Write a word into the world").fill(text);
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("word-status")).toHaveText(`“${text}” is in the scene`);
  }
  const img = await page.evaluate(() => window.__fleet!.view().layout!.image);
  for (const [u, v] of [[0.18, 0.3], [0.62, 0.22], [0.84, 0.62], [0.35, 0.7]] as const) {
    await page.getByRole("button", { name: "Drop a light" }).click();
    await page.mouse.click(img.x + u * img.w, img.y + v * img.h);
  }
  await page.evaluate(() => window.__fleet!.client.send("time_of_day", { v: 0.65 }));
  await page.waitForTimeout(1500);
  const bar = { x: page.viewportSize()!.width / 2, y: page.viewportSize()!.height - 5 };
  await page.mouse.move(bar.x, bar.y);
  await shot(page, "wall-desktop");

  const b = await tileBox(page, "slot-3");
  await page.mouse.move(b.x + b.w / 2, b.y + b.h / 2);
  await page.waitForTimeout(400);
  await shot(page, "wall-hover");
  await page.mouse.move(bar.x, bar.y);

  await page.getByTestId("stats-switch").click();
  await page.waitForTimeout(1200);
  await shot(page, "wall-stats");
  await page.getByTestId("stats-switch").click();

  await page.getByTestId("relaunch-all").click();
  await expect(page.getByTestId("stamp-fcvm")).toHaveText(/^all back at/, { timeout: 10_000 });
  await page.waitForTimeout(1200);
  await shot(page, "race-mid");

  const o = await tileBox(page, "slot-6");
  await page.mouse.click(o.x + o.w / 2, o.y + o.h / 2);
  await expect(page.getByTestId("focus-stream")).toContainText(/^\d+p · \d+ fps/, { timeout: 15_000 });
  await page.waitForTimeout(1500);
  await shot(page, "tile-opened");
  await page.getByRole("button", { name: "Back to the wall" }).click();

  // Aggregated: two joined streams, the same tile state and hover on top.
  await page.getByTestId("mode-aggregated").click();
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.getState().ui.shownMode), { timeout: 20_000 }).toBe("aggregated");
  await page.getByTestId("stats-switch").click();
  const h = await tileBox(page, "ec2-2");
  await page.mouse.move(h.x + h.w / 2, h.y + h.h / 2);
  await page.waitForTimeout(800);
  await shot(page, "wall-aggregated");
  await page.getByTestId("stats-switch").click();
  await page.getByTestId("mode-direct").click();
  await page.mouse.move(bar.x, bar.y);

  const ctx = await browser.newContext({ ...devices["iPhone 13"] });
  const phone = await ctx.newPage();
  await openWall(phone, 7);
  await phone.waitForTimeout(800);
  await shot(phone, "phone");
  await ctx.close();

  // Leave the fleet at rest for the next file.
  await expect(page.getByTestId("stamp-ec2")).toHaveText(/^last relaunch all/, { timeout: 30_000 });
  await page.evaluate(() => window.__fleet!.client.send("time_of_day", { v: 0.42 }));
});
