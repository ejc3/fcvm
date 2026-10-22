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
  for (const [u, v] of [[0.18, 0.3], [0.62, 0.22], [0.84, 0.62], [0.35, 0.7]] as const) {
    await page.evaluate(([pu, pv]) => window.__fleet!.client.send("light", { pos: { u: pu, v: pv }, color: [1, 0.85, 0.5] }), [u, v]);
  }
  await page.evaluate(() => window.__fleet!.client.send("time_of_day", { v: 15.5 }));
  await page.waitForTimeout(1500);
  await page.mouse.move(720, 895);
  await shot(page, "wall-desktop");

  const b = await tileBox(page, "slot-3");
  await page.mouse.move(b.x + b.w / 2, b.y + b.h / 2);
  await page.waitForTimeout(400);
  await shot(page, "wall-hover");
  await page.mouse.move(720, 895);

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
  await expect(page.getByTestId("focus-stream")).toContainText(/^(540|720|1080)p/, { timeout: 15_000 });
  await page.waitForTimeout(1500);
  await shot(page, "tile-opened");
  await page.getByRole("button", { name: "Back to the wall" }).click();

  const ctx = await browser.newContext({ ...devices["iPhone 13"] });
  const phone = await ctx.newPage();
  await openWall(phone, 7);
  await phone.waitForTimeout(800);
  await shot(phone, "phone");
  await ctx.close();

  // Leave the fleet at rest for the next file.
  await expect(page.getByTestId("stamp-ec2")).toHaveText(/^last relaunch all/, { timeout: 30_000 });
  await page.evaluate(() => window.__fleet!.client.send("time_of_day", { v: 10 }));
});
