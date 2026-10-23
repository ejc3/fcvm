// The pictures for the PR and the README: the stage at rest, hover, the Play
// drawer, a race mid-way (with EC2 waiting for spot capacity) and after it, an
// opened VM, a viewer's own GPU and the tile it borrowed, the failure states and
// a phone. Written to FLEET_SCREENSHOT_DIR when set, else to the test's output folder.

import { expect, test, devices, type Page } from "@playwright/test";
import { admin, openWall, raceOver } from "./helpers.ts";

async function shot(page: Page, name: string): Promise<void> {
  const dir = process.env.FLEET_SCREENSHOT_DIR ?? test.info().outputPath();
  await page.screenshot({ path: `${dir}/${name}.jpg`, type: "jpeg", quality: 86 });
}

const tileBox = (page: Page, id: string) => page.evaluate((n) => window.__fleet!.view().tiles.find((t) => t.id === n)!.box!, id);

test("screenshots", async ({ page, browser }) => {
  test.setTimeout(240_000);
  await raceOver();
  await openWall(page, 3, "aggregated");
  const away = { x: page.viewportSize()!.width - 5, y: page.viewportSize()!.height - 5 };

  // Give the scene some viewers: words, lights, an evening sun.
  await page.getByTestId("play").click();
  for (const text of ["hello fleet", "fcvm"]) {
    await page.getByLabel("Write a word into the world").fill(text);
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("word-status")).toHaveText(`“${text}” is in the world`);
  }
  await page.getByTestId("nudge-nebula").click();
  await page.waitForTimeout(300);
  await shot(page, "play-drawer");
  for (const [u, v] of [[0.18, 0.3], [0.62, 0.22], [0.84, 0.62], [0.35, 0.7]] as const) {
    if ((await page.getByTestId("drawer").count()) === 0) await page.getByTestId("play").click();
    await page.getByTestId("light").click();
    const img = await page.evaluate(() => window.__fleet!.view().layout!.image);
    await page.mouse.click(img.x + u * img.w, img.y + v * img.h);
  }
  await page.evaluate(() => window.__fleet!.client.send("time_of_day", { v: 0.65 }));

  // A race with a wait for spot capacity, mid-way: fcvm back, EC2 waiting and hatched.
  await admin("/mock/schedule", { capacity_wait_ms: 5000, clear_cooldown: true, cooldown_s: 45 });
  await page.getByTestId("restart-all").click();
  await expect(page.getByTestId("bar-fcvm")).toHaveClass(/done/, { timeout: 10_000 });
  await page.mouse.move(away.x, away.y);
  await page.waitForTimeout(400);
  await shot(page, "race-mid");
  await raceOver();
  await page.waitForTimeout(2500);
  await shot(page, "race-after");
  await admin("/mock/schedule", { clear_cooldown: true, cooldown_s: 0, next_in_ms: 41_000 });
  await page.waitForTimeout(1500);
  await shot(page, "wall-desktop");
  await page.getByTestId("isolation-link").click();
  await expect(page.locator("#isolation-title")).toBeFocused();
  await page.waitForTimeout(300);
  await shot(page, "isolation");
  await page.getByRole("button", { name: "Close" }).click();

  const b = await tileBox(page, "ec2:3");
  await page.mouse.move(b.x + b.w / 2, b.y + b.h / 2);
  await page.waitForTimeout(400);
  await shot(page, "wall-hover");
  await page.mouse.move(away.x, away.y);

  const o = await tileBox(page, "fcvm:6");
  await page.mouse.click(o.x + o.w / 2, o.y + o.h / 2);
  await expect(page.getByTestId("focus-stream")).toContainText(/^\d+p · \d+ fps/, { timeout: 15_000 });
  await page.waitForTimeout(1500);
  await shot(page, "tile-opened");
  await page.getByRole("button", { name: "Back to the wall" }).click();

  // A viewer's own GPU, and the wall tile it borrowed as another viewer sees it.
  const watcher = await (await browser.newContext()).newPage();
  await openWall(watcher, 3, "aggregated");
  await page.getByTestId("own-gpu").click();
  await expect(page.getByTestId("own-status")).toHaveText("Your GPU is streaming", { timeout: 10_000 });
  await page.waitForTimeout(1500);
  await shot(page, "own-gpu");
  await watcher.mouse.move(away.x, away.y);
  await shot(watcher, "own-gpu-borrowed-tile");
  await expect(page.getByTestId("own")).toHaveCount(0, { timeout: 20_000 });
  await watcher.context().close();
  // The borrowed tile restores back in; its label goes a few seconds after.
  await expect(page.getByTestId("hover-label")).toHaveCount(0, { timeout: 20_000 });
  await admin("/mock/schedule", { next_in_ms: null });

  // Failure states: a VM that did not come back, a reclaimed host, then the coordinator offline.
  await admin("/mock/fault", { fail: "fcvm:2", message: "GPU did not come back after reset", retry_ms: 20_000 });
  await admin("/mock/fault", { reclaim: "mock-host-2" });
  await page.waitForTimeout(800);
  await shot(page, "failure-failed-and-reclaimed");
  await admin("/mock/offline", { ms: 6000 });
  await expect(page.getByTestId("banner-offline")).toBeVisible();
  await page.waitForTimeout(500);
  await shot(page, "failure-offline");
  await expect(page.getByTestId("banner-offline")).toHaveCount(0, { timeout: 20_000 });
  await admin("/mock/fault", { replace_host: "mock-host-2" });

  const ctx = await browser.newContext({ ...devices["iPhone 13"], deviceScaleFactor: 1 });
  const phone = await ctx.newPage();
  await openWall(phone, 3, "aggregated");
  await phone.waitForTimeout(800);
  await shot(phone, "phone");
  await ctx.close();

  // Leave the fleet at rest.
  await expect(page.getByTestId("dark-fcvm:2")).toHaveCount(0, { timeout: 30_000 });
  await page.evaluate(() => window.__fleet!.client.send("time_of_day", { v: 0.42 }));
});
