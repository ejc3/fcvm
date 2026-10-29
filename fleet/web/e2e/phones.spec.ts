// Phones mode (`?mode=phones`) against the mock fleet with four phones
// (playwright.phones.config.ts): the seamless picture, the zoom every viewer shares, an opened
// phone, taking it over and its fixed actions. The wall at `/` keeps its own suite.

import { expect, test, type Browser, type Page } from "@playwright/test";
import { COORD, admin, mockState } from "./helpers.ts";

const PHONES = 4;

const view = (page: Page) => page.evaluate(() => window.__phones!.view());
const bezels = (page: Page) => page.getByTestId("phone-bezel");

type MockPhones = { zoomed_out: boolean; tiles: { id: string; phase: string; borrowed: { session: string; viewer: string } | null; last_relaunch: { by: string; first_frame_at: number | null } }[] };
const mockPhones = async () => ((await mockState()).state as unknown as { phones: MockPhones }).phones;
const phoneActions = async () => ((await (await fetch(`${COORD}/mock/phone-actions`)).json()) as { actions: { action: string; phone: string; session: string }[] }).actions;

/** Open the phones page and wait until every phone draws at the common t. */
async function openPhones(page: Page, synced = PHONES): Promise<void> {
  await page.goto("/?mode=phones&debug=1");
  await page.waitForFunction(() => window.__phones !== undefined);
  await expect.poll(async () => page.evaluate(() => window.__phones!.client.bench?.tilesSynced ?? 0), { timeout: 30_000 }).toBeGreaterThanOrEqual(synced);
}

async function viewer(browser: Browser, width = 1024, height = 640): Promise<Page> {
  const page = await (await browser.newContext({ viewport: { width, height } })).newPage();
  await openPhones(page);
  return page;
}

test.describe.configure({ mode: "serial" });

test.beforeEach(async () => {
  await admin("/mock/phones", { release_all: true, zoomed_out: false });
  await expect.poll(async () => (await mockPhones()).tiles.every((t) => t.phase === "running"), { timeout: 15_000 }).toBe(true);
});

test("shows the four phones seamless: one picture, every phone drawn at the common t, no bezel", async ({ page }) => {
  await openPhones(page);
  const v = await view(page);
  expect(v.title).toBe("Phones");
  expect(v.plan.image).toBeNull();
  expect(v.plan.direct.map((d) => d.id)).toEqual(["d:phone:1", "d:phone:2", "d:phone:3", "d:phone:4"]);
  // Neighbours share edges: no gap between one phone's screen and the next.
  const boxes = v.layout!.phones.map((p) => p.box!);
  for (let i = 1; i < boxes.length; i++) expect(boxes[i]!.x).toBe(boxes[i - 1]!.x + boxes[i - 1]!.w);
  expect(boxes[0]!.x).toBe(v.layout!.image.x);
  await expect(bezels(page)).toHaveCount(0);
  await expect(page.getByTestId("phones-zoom")).toHaveText("Zoom out");
  // Each phone drew a frame at the same t, and the picture is not black.
  const shot = await page.evaluate(
    (bs) => window.__phones!.client.capture(bs.map((b) => ({ x: b.x + b.w / 2, y: b.y + b.h / 2 }))),
    boxes,
  );
  for (let i = 1; i <= PHONES; i++) expect(shot.shown[`d:phone:${i}`], `phone ${i}`).toMatch(/^sync@/);
  expect(shot.pixels.some((px) => px[0] + px[1] + px[2] > 60)).toBe(true);
});

test("the wall at / has no phones in it", async ({ page }) => {
  await page.goto("/?debug=1");
  await page.waitForFunction(() => window.__fleet !== undefined);
  await expect(page.getByTestId("stage")).toBeVisible();
  await expect(page.getByTestId("phones")).toHaveCount(0);
  await expect(page.getByTestId("phones-zoom")).toHaveCount(0);
  expect(await page.evaluate(() => window.__phones === undefined)).toBe(true);
  // The wall's tiles are the wall's: the phones are not among them.
  const ids = await page.evaluate(() => window.__fleet!.view().tiles.map((t) => t.id));
  expect(ids.some((id) => id.startsWith("phone"))).toBe(false);
  await page.goto("/?mode=other&debug=1");
  await page.waitForFunction(() => window.__fleet !== undefined);
});

test("Zoom out moves the phones apart into bezels, each labelled with its restore, and Zoom in puts them back", async ({ page }) => {
  await openPhones(page);
  await page.getByTestId("phones-zoom").click();
  await expect(bezels(page)).toHaveCount(PHONES);
  await expect(page.getByTestId("phones")).toHaveAttribute("data-progress", "1.000");
  await expect(page.getByTestId("phones-zoom")).toHaveText("Zoom in");
  expect((await mockPhones()).zoomed_out).toBe(true);
  const v = await view(page);
  const L = v.layout!;
  const bodies = L.phones.map((p) => p.bezel!);
  for (let i = 0; i < bodies.length; i++) {
    for (let j = i + 1; j < bodies.length; j++) {
      const a = bodies[i]!;
      const b = bodies[j]!;
      expect(a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h, `bodies ${i} and ${j} overlap`).toBe(false);
    }
  }
  // Each screen keeps its frame's aspect: the stream's own size (the mock rounds a width to an even number).
  const sizes = await page.evaluate(() => Object.fromEntries(Object.entries(window.__phones!.client.getState().frames!.tiles).map(([k, t]) => [k, t.width / t.height])));
  for (const p of L.phones) {
    expect(p.box!.w / p.box!.h).toBeCloseTo(sizes[`d:${p.id}`]!, 2);
    expect(p.box!.w / p.box!.h).toBeCloseTo(9 / 16, 1);
  }
  // The stream still draws in the moved box.
  await expect.poll(async () => page.evaluate(() => window.__phones!.client.bench?.tilesSynced ?? 0)).toBe(PHONES);
  await expect(page.getByTestId("phone-label-phone:2")).toContainText(/^Phone 2 · restored in \d+\.\d s$/);
  // The coordinator takes one zoom per viewer every 2 s (FLEET_MOCK_PHONE_ZOOM_GAP_MS).
  await page.waitForTimeout(2100);
  await page.getByTestId("phones-zoom").click();
  await expect(bezels(page)).toHaveCount(0);
  await expect(page.getByTestId("phones-zoom")).toHaveText("Zoom out");
});

test("the zoom is every viewer's: a second page sees the bezels when the first zooms out, and the first sees them go", async ({ page, browser }) => {
  await openPhones(page);
  const other = await viewer(browser);
  await expect(bezels(other)).toHaveCount(0);
  await page.getByTestId("phones-zoom").click();
  await expect(bezels(page)).toHaveCount(PHONES);
  await expect(bezels(other)).toHaveCount(PHONES);
  await expect(other.getByTestId("phones-zoom")).toHaveText("Zoom in");
  await other.getByTestId("phones-zoom").click();
  await expect(bezels(other)).toHaveCount(0);
  await expect(bezels(page)).toHaveCount(0);
  await other.context().close();
});

test("a zoom sent twice in a row is refused with the coordinator's reason", async ({ page }) => {
  await openPhones(page);
  await page.evaluate(() => {
    const c = window.__phones!.client;
    c.send("phones_zoom", { out: true });
    c.send("phones_zoom", { out: false });
  });
  await expect(page.getByTestId("phone-notice")).toHaveText(/^one zoom every 2 s; wait \d+ ms$/);
  expect((await mockPhones()).zoomed_out).toBe(true);
});

test("opening a phone shows its restore time and details beside it, other phones stop streaming, Escape returns", async ({ page }) => {
  await openPhones(page);
  await page.getByTestId("phone-hit-phone:2").click();
  const panel = page.getByTestId("phone-panel");
  await expect(panel).toBeVisible();
  await expect(panel.getByRole("heading", { name: "Phone 2" })).toBeVisible();
  await expect(page.getByTestId("phone-state")).toHaveText(/^Phone restored in \d+\.\d s$/);
  await expect(page.getByTestId("phone-rows")).toContainText("fleet-phone-2");
  await expect(page.getByTestId("timeline")).toContainText("snapshot restored");
  await expect(bezels(page)).toHaveCount(1);
  const v = await view(page);
  expect(v.plan.direct.map((d) => d.id)).toEqual(["d:phone:2"]);
  // On the left of the stage, clear of the panel.
  const panelBox = (await panel.boundingBox())!;
  const open = v.layout!.phones.find((p) => p.id === "phone:2")!;
  expect(open.bezel!.x + open.bezel!.w).toBeLessThanOrEqual(panelBox.x);
  await expect.poll(async () => page.evaluate(() => window.__phones!.client.bench?.tilesSynced ?? 0), { timeout: 15_000 }).toBe(1);
  await page.keyboard.press("Escape");
  await expect(panel).toHaveCount(0);
  await expect.poll(async () => (await view(page)).plan.direct.length).toBe(PHONES);
  await expect(page.getByTestId("phone-hit-phone:2")).toBeFocused();
});

test("taking a phone over gives the eight fixed actions; Rotate reaches the phone, and a bad name is refused", async ({ page }) => {
  await openPhones(page);
  await page.getByTestId("phone-hit-phone:3").click();
  await expect(page.getByTestId("phone-borrow")).toHaveAttribute("data-kind", "free");
  await page.getByTestId("phone-take").click();
  await expect(page.getByTestId("phone-borrow")).toHaveAttribute("data-kind", "yours");
  await expect(page.getByTestId("phone-countdown")).toHaveText(/^Back in \d+ s$/);
  const labels = await page.getByTestId("phone-borrow").getByRole("group", { name: "Actions" }).getByRole("button").allTextContents();
  expect(labels).toEqual(["Switch screen", "Rotate", "Dark mode", "Dance", "Wave", "GPU benchmark", "Restart app", "Reboot phone"]);
  const session = (await mockPhones()).tiles[2]!.borrowed!.session;
  await page.getByTestId("phone-action-rotate").click();
  await expect.poll(async () => (await phoneActions()).map((a) => [a.action, a.phone, a.session])).toEqual([["rotate", "phone-3", session]]);
  // Only the enumerated names get through, whatever the page sends.
  await page.evaluate(() => window.__phones!.client.send("phone_action", { action: "adb shell rm -rf /" }));
  await expect(page.getByTestId("phone-notice")).toHaveText("args.action must be one of switch_screen, rotate, dark_mode, dance, wave, gpu_benchmark, restart_app, reboot_phone");
  expect(await phoneActions()).toHaveLength(1);
  await page.getByTestId("phone-give-back").click();
  await expect.poll(async () => (await mockPhones()).tiles[2]!.borrowed).toBeNull();
});

test("Reboot phone restores the phone: the panel says Restoring, the stream returns, and the restore time updates", async ({ page }) => {
  await openPhones(page);
  await page.getByTestId("phone-hit-phone:1").click();
  await page.getByTestId("phone-take").click();
  await expect(page.getByTestId("phone-borrow")).toHaveAttribute("data-kind", "yours");
  const before = await page.getByTestId("phone-restore-total").textContent();
  expect(before).toMatch(/^\d+\.\d s$/);
  await page.getByTestId("phone-action-reboot_phone").click();
  await expect(page.getByTestId("phone-state")).toHaveText("Restoring");
  await expect(page.getByTestId("phone-action-rotate")).toBeDisabled();
  await expect(page.getByTestId("phone-state")).toHaveText(/^Phone restored in \d+\.\d s$/, { timeout: 15_000 });
  const after = await page.getByTestId("phone-restore-total").textContent();
  expect(after).not.toBe(before);
  // The mock restores in about 2 s.
  expect(Number(after!.replace(" s", ""))).toBeGreaterThan(1.5);
  expect(Number(after!.replace(" s", ""))).toBeLessThan(6);
  await expect(page.getByTestId("phone-action-rotate")).toBeEnabled();
  expect((await mockPhones()).tiles[0]!.last_relaunch.by).toMatch(/^reboot:phone-claim-\d+$/);
  // Its stream is back and drawing.
  await expect.poll(async () => page.evaluate(() => window.__phones!.client.bench?.tilesSynced ?? 0), { timeout: 15_000 }).toBe(1);
  await page.getByTestId("phone-give-back").click();
  await expect(page.getByTestId("phone-borrow")).toHaveAttribute("data-kind", "free");
  await expect.poll(async () => (await mockPhones()).tiles[0]!.last_relaunch.by).toMatch(/^return:phone-claim-\d+$/);
});

test("a phone lent to another viewer shows no actions; giving it back returns it", async ({ page, browser }) => {
  await openPhones(page);
  const other = await viewer(browser);
  await page.getByTestId("phone-hit-phone:4").click();
  await page.getByTestId("phone-take").click();
  await expect(page.getByTestId("phone-borrow")).toHaveAttribute("data-kind", "yours");
  await other.getByTestId("phone-hit-phone:4").click();
  await expect(other.getByTestId("phone-lent")).toHaveText(/^Borrowed by another viewer until \d\d:\d\d:\d\d UTC$/);
  await expect(other.getByTestId("phone-take")).toHaveCount(0);
  await expect(other.getByRole("group", { name: "Actions" })).toHaveCount(0);
  await page.getByTestId("phone-give-back").click();
  await expect(other.getByTestId("phone-borrow")).toHaveAttribute("data-kind", "free", { timeout: 10_000 });
  await other.context().close();
});

test("a second borrow by the same viewer is refused with the coordinator's reason", async ({ page }) => {
  await openPhones(page);
  await page.getByTestId("phone-hit-phone:1").click();
  await page.getByTestId("phone-take").click();
  await expect(page.getByTestId("phone-borrow")).toHaveAttribute("data-kind", "yours");
  await page.evaluate(() => window.__phones!.client.send("claim_phone", { phone: "phone-2" }));
  await expect(page.getByTestId("phone-notice")).toHaveText("one phone per client at a time");
  await page.getByTestId("phone-give-back").click();
});

test("on a phone screen the phones fit and an opened phone has a bottom sheet under it", async ({ browser }) => {
  const page = await viewer(browser, 390, 780);
  await page.getByTestId("phones-zoom").click();
  await expect(bezels(page)).toHaveCount(PHONES);
  await expect(page.getByTestId("phones")).toHaveAttribute("data-progress", "1.000");
  const v = await view(page);
  for (const p of v.layout!.phones) {
    expect(p.bezel!.x).toBeGreaterThanOrEqual(0);
    expect(p.bezel!.x + p.bezel!.w).toBeLessThanOrEqual(390);
  }
  await page.getByTestId("phone-hit-phone:2").click();
  const panel = page.getByTestId("phone-panel");
  await expect(panel).toHaveClass(/narrow/);
  const sheet = (await panel.boundingBox())!;
  const open = (await view(page)).layout!.phones.find((p) => p.id === "phone:2")!;
  expect(open.label!.y + open.label!.h).toBeLessThanOrEqual(sheet.y + 1);
  expect(sheet.x).toBe(0);
  expect(sheet.width).toBe(390);
  // A refusal while the sheet is open is read above it, not under it.
  await page.getByTestId("phone-take").click();
  await expect(page.getByTestId("phone-borrow")).toHaveAttribute("data-kind", "yours");
  await page.evaluate(() => window.__phones!.client.send("claim_phone", { phone: "phone-1" }));
  const notice = page.getByTestId("phone-notice");
  await expect(notice).toHaveText("one phone per client at a time");
  const nb = (await notice.boundingBox())!;
  const top = await page.evaluate(([x, y]) => document.elementFromPoint(x!, y!)?.closest("[data-testid]")?.getAttribute("data-testid"), [nb.x + nb.width / 2, nb.y + nb.height / 2]);
  expect(top).toBe("phone-notice");
  await page.getByTestId("phone-give-back").click();
  await page.getByTestId("phone-back").click();
  await expect(panel).toHaveCount(0);
  await page.context().close();
});

// The pictures for the PR: written to FLEET_SCREENSHOT_DIR when set, else to the test's output folder.
test("pictures: seamless, zoomed out, and a phone taken over", async ({ page }) => {
  const shot = async (name: string) => {
    const dir = process.env.FLEET_SCREENSHOT_DIR ?? test.info().outputPath();
    await page.screenshot({ path: `${dir}/${name}.jpg`, type: "jpeg", quality: 86 });
  };
  await openPhones(page);
  await page.mouse.move(2, 2);
  await shot("phones-seamless");
  await page.getByTestId("phones-zoom").click();
  await expect(page.getByTestId("phones")).toHaveAttribute("data-progress", "1.000");
  await page.waitForTimeout(600);
  await shot("phones-zoomed-out");
  await page.getByTestId("phone-hit-phone:2").click();
  await page.getByTestId("phone-take").click();
  await expect(page.getByTestId("phone-borrow")).toHaveAttribute("data-kind", "yours");
  await page.getByTestId("phone-action-dance").click();
  await page.waitForTimeout(800);
  await shot("phones-taken-over");
  await page.getByTestId("phone-give-back").click();
});
