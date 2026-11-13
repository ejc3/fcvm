// Windows mode (`?mode=windows`) against the mock fleet with four sandboxes
// (playwright.windows.config.ts), on a desktop screen and on a phone: the desktops drawn from
// their streams, what each tile says, reset one and reset all, taking a sandbox over and its
// fixed actions, and that nothing on the page sends anything but the four Windows intents.

import { expect, test, type Browser, type Page } from "@playwright/test";
import { COORD, admin, mockState } from "./helpers.ts";

const SANDBOXES = 4;
const SLOTS = ["win-1", "win-2", "win-3", "win-4"];
const LABEL = "Windows Server 2025 evaluation, for demonstration only";
const EDITION = "Windows Server 2025 Datacenter Evaluation (Desktop Experience)";
const ACTIONS = ["Start menu", "Notepad", "File Explorer", "Task Manager", "About Windows", "PowerShell", "Close all", "Reset"];
const INTENTS = ["windows_reset", "claim_windows", "release_windows", "windows_action"];

const view = (page: Page) => page.evaluate(() => window.__windows!.view());

type MockWindows = {
  last_reset_all: { sandboxes: number; by: string } | null;
  tiles: { id: string; slot: string; phase: string; borrowed: { session: string; viewer: string } | null; last_relaunch: { by: string; clicked_at: number; first_frame_at: number | null } }[];
};
const mockWindows = async () => ((await mockState()).state as unknown as { windows: MockWindows }).windows;
const record = async () =>
  (await (await fetch(`${COORD}/mock/windows-actions`)).json()) as {
    actions: { action: string; sandbox: string; session: string }[];
    intents: { name: string; outcome: string; args: Record<string, unknown> }[];
  };
const seconds = (text: string | null) => Number(/(\d+\.\d+) s/.exec(text ?? "")![1]);

/** Open the Windows page and wait until every sandbox draws at the common t. */
async function openWindows(page: Page, synced = SANDBOXES): Promise<void> {
  await page.goto("/?mode=windows&debug=1");
  await page.waitForFunction(() => window.__windows !== undefined);
  await expect.poll(async () => page.evaluate(() => window.__windows!.client.bench?.tilesSynced ?? 0), { timeout: 30_000 }).toBeGreaterThanOrEqual(synced);
}

async function viewer(browser: Browser): Promise<Page> {
  const page = await (await browser.newContext({ viewport: { width: 1024, height: 640 } })).newPage();
  await openWindows(page);
  return page;
}

const hit = (page: Page, slot: string) => page.getByTestId(`sandbox-hit-${slot}`);

test.describe.configure({ mode: "serial" });

test.beforeEach(async () => {
  await admin("/mock/windows", { release_all: true, image_age_days: 12 });
  await expect.poll(async () => (await mockWindows()).tiles.every((t) => t.phase === "running"), { timeout: 15_000 }).toBe(true);
});

/** How many elements on the page show `text` as their own words (not through a child). */
const shown = (page: Page, text: string) =>
  page.evaluate((text) => [...document.querySelectorAll("body *")].filter((e) => [...e.childNodes].some((n) => n.nodeType === Node.TEXT_NODE && (n.textContent ?? "").includes(text))).length, text);

test("shows the sandboxes, each drawing frames, says each one's slot and time to desktop, and the edition and the label once", async ({ page }) => {
  await openWindows(page);
  const v = await view(page);
  expect(v.title).toBe("Windows");
  // The tab says which mode this is (the wall's title was on every mode's tab).
  await expect(page).toHaveTitle("Windows");
  expect(v.plan.image).toBeNull();
  expect(v.plan.direct.map((d) => d.id)).toEqual(["d:win:1", "d:win:2", "d:win:3", "d:win:4"]);
  // Each desktop drew a frame at the same t, and the picture is not black.
  const boxes = v.cards.map((c) => c.box!);
  const shot = await page.evaluate((bs) => window.__windows!.client.capture(bs.map((b) => ({ x: b.x + b.w / 2, y: b.y + b.h / 2 }))), boxes);
  for (let i = 1; i <= SANDBOXES; i++) expect(shot.shown[`d:win:${i}`], `win-${i}`).toMatch(/^(fade@)?sync@/);
  expect(shot.pixels.some((px) => px[0] + px[1] + px[2] > 60)).toBe(true);
  // Each is 16:9 and on screen, and the page does not scroll sideways.
  for (const b of boxes) expect(b.w / b.h).toBeCloseTo(16 / 9, 1);
  for (const slot of SLOTS) await expect(hit(page, slot)).toBeInViewport();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  for (const slot of SLOTS) {
    await expect(page.getByTestId(`sandbox-slot-${slot}`)).toHaveText(slot);
    await expect(page.getByTestId(`sandbox-desktop-in-${slot}`)).toHaveText(/^Desktop in \d+\.\d\d s$/);
    await expect(page.getByTestId(`sandbox-link-${slot}`)).toHaveCount(0);
  }
  // The edition and the evaluation notice: once each, in the header.
  await expect(page.getByTestId("win-image")).toContainText(EDITION);
  await expect(page.getByTestId("win-label")).toHaveText(LABEL);
  await expect(page.getByTestId("win-label")).toBeInViewport();
  expect(await shown(page, EDITION)).toBe(1);
  expect(await shown(page, LABEL)).toBe(1);
  // The header: sandboxes up, the median and the slowest time to desktop, the last reset all.
  await expect(page.getByTestId("win-up")).toHaveText("4 of 4 sandboxes up");
  await expect(page.getByTestId("win-median")).toHaveText(/^Median time to desktop \d+\.\d\d s$/);
  await expect(page.getByTestId("win-slowest")).toHaveText(/^Slowest \d+\.\d\d s$/);
  await expect(page.getByTestId("win-last-reset-all")).toHaveText("Reset all: not used yet");
  await expect(page.getByTestId("win-image")).toContainText("build 26100.1742");
  await expect(page.getByTestId("win-warning")).toHaveCount(0);
  await expect(page.getByTestId("win-refused")).toHaveCount(0);
  await expect(page.getByTestId("win-panel")).toHaveCount(0);
});

test("the wall at / has no sandboxes in it, and the mode switch reaches Windows from both", async ({ page }) => {
  await page.goto("/?debug=1");
  await page.waitForFunction(() => window.__fleet !== undefined);
  await expect(page.getByTestId("windows")).toHaveCount(0);
  expect(await page.evaluate(() => window.__windows === undefined)).toBe(true);
  const ids = await page.evaluate(() => window.__fleet!.view().tiles.map((t) => t.id));
  expect(ids.some((id) => id.startsWith("win"))).toBe(false);
  await expect(page.getByTestId("mode-windows")).toHaveAttribute("href", "/?debug=1&mode=windows");
  await expect(page.getByTestId("mode-windows")).toBeInViewport();
  await page.getByTestId("mode-windows").click();
  await page.waitForFunction(() => window.__windows !== undefined);
  await expect(page.getByTestId("mode-windows")).toHaveAttribute("aria-current", "page");
  await expect(page.getByTestId("mode-wall")).toBeInViewport();
  await page.getByTestId("mode-wall").click();
  await page.waitForFunction(() => window.__fleet !== undefined);
  await expect(page).toHaveURL(/\/\?debug=1$/);
});

test("clicking a tile throws its sandbox away: it goes dark with a running timer and comes back with a new Desktop in", async ({ page }) => {
  await openWindows(page);
  const clickedBefore = (await mockWindows()).tiles[1]!.last_relaunch.clicked_at;
  await hit(page, "win-2").click();
  await expect(page.getByTestId("sandbox-dark-win-2")).toBeVisible();
  const timer = page.getByTestId("sandbox-timer-win-2");
  await expect(timer).toHaveText(/^(Restoring|Connecting) \d+\.\d s$/);
  await expect(page.getByTestId("sandbox-desktop-in-win-2")).toHaveCount(0);
  // the timer runs
  const first = seconds(await timer.textContent());
  await expect.poll(async () => seconds(await timer.textContent().catch(() => null)), { timeout: 5000 }).toBeGreaterThan(first);
  // the others keep their desktops
  await expect(page.getByTestId("sandbox-dark-win-1")).toHaveCount(0);
  expect((await mockWindows()).tiles[1]!.last_relaunch.by).toMatch(/^viewer:/);
  // back: live again, with the restore's own time (the mock takes 3 s), and its stream drawing
  await expect(hit(page, "win-2")).toHaveAttribute("data-phase", "live", { timeout: 15_000 });
  const after = seconds(await page.getByTestId("sandbox-desktop-in-win-2").textContent());
  expect(after).toBeGreaterThanOrEqual(2.9);
  expect((await mockWindows()).tiles[1]!.last_relaunch.clicked_at).toBeGreaterThan(clickedBefore);
  await expect.poll(async () => page.evaluate(() => window.__windows!.client.bench?.tilesSynced ?? 0), { timeout: 15_000 }).toBe(SANDBOXES);
});

test("two resets in a row from one client: the second is refused with the coordinator's reason", async ({ page }) => {
  await openWindows(page);
  await hit(page, "win-1").click();
  await hit(page, "win-3").click();
  await expect(page.getByTestId("win-notice")).toHaveText("one reset per 2 s per client");
  await expect(page.getByTestId("sandbox-dark-win-3")).toHaveCount(0);
  await expect(hit(page, "win-1")).toHaveAttribute("data-phase", "live", { timeout: 15_000 });
});

test("Reset all resets every sandbox, shows each one's timer, and the header gives the last reset all, the median and the slowest", async ({ page }) => {
  await openWindows(page);
  await page.getByTestId("win-reset-all").click();
  for (const slot of SLOTS) await expect(page.getByTestId(`sandbox-timer-${slot}`)).toHaveText(/^(Restoring|Connecting) \d+\.\d s$/);
  await expect(page.getByTestId("win-up")).toHaveText("0 of 4 sandboxes up");
  expect((await mockWindows()).tiles.map((t) => t.last_relaunch.by)).toEqual(["reset_all", "reset_all", "reset_all", "reset_all"]);
  for (const slot of SLOTS) await expect(hit(page, slot)).toHaveAttribute("data-phase", "live", { timeout: 20_000 });
  await expect(page.getByTestId("win-up")).toHaveText("4 of 4 sandboxes up");
  await expect(page.getByTestId("win-last-reset-all")).toHaveText(/^Last reset all: \d\d:\d\d:\d\d UTC, 4 sandboxes, by you$/);
  expect((await mockWindows()).last_reset_all).toMatchObject({ sandboxes: 4 });
  for (const slot of SLOTS) expect(seconds(await page.getByTestId(`sandbox-desktop-in-${slot}`).textContent())).toBeGreaterThanOrEqual(2.9);
  const median = seconds(await page.getByTestId("win-median").textContent());
  const slowest = seconds(await page.getByTestId("win-slowest").textContent());
  expect(median).toBeGreaterThanOrEqual(2.9);
  expect(slowest).toBeGreaterThanOrEqual(median);
  // once per 10 s per client
  await page.getByTestId("win-reset-all").click();
  await expect(page.getByTestId("win-notice")).toHaveText("one reset all per 10 s per client");
});

test("another viewer sees the last reset all as another viewer's", async ({ page, browser }) => {
  await openWindows(page);
  const other = await viewer(browser);
  await page.getByTestId("win-reset-all").click();
  await expect(other.getByTestId("win-last-reset-all")).toHaveText(/^Last reset all: \d\d:\d\d:\d\d UTC, 4 sandboxes, by another viewer$/);
  await expect(page.getByTestId("win-last-reset-all")).toContainText("by you");
  for (const slot of SLOTS) await expect(hit(page, slot)).toHaveAttribute("data-phase", "live", { timeout: 20_000 });
  await other.context().close();
});

test("taking a sandbox over gives exactly the fixed actions and Reset; an action is accepted, the label stays once on the page, and giving it back ends the borrow", async ({ page }) => {
  await openWindows(page);
  await page.getByTestId("sandbox-take-win-3").click();
  const panel = page.getByTestId("win-panel");
  await expect(panel).toBeVisible();
  await expect(page.getByTestId("win-panel-title")).toHaveText("win-3");
  await expect(page.getByTestId("win-countdown")).toHaveText(/^Returns in \d+ s$/);
  await expect(page.getByTestId("win-label")).toBeInViewport();
  expect(await shown(page, LABEL)).toBe(1);
  expect(await page.getByTestId("win-actions").getByRole("button").allTextContents()).toEqual(ACTIONS);
  // no field to type in, anywhere on the page
  await expect(page.locator("input, textarea, select, [contenteditable]")).toHaveCount(0);
  const session = (await mockWindows()).tiles[2]!.borrowed!.session;
  await expect(page.getByTestId("sandbox-caption-win-3")).toHaveAttribute("data-mine", "true");
  // the held sandbox cannot be reset by a click, and no other can be taken
  await expect(hit(page, "win-3")).toBeDisabled();
  if (!(await page.getByTestId("sandbox-take-win-1").isHidden())) await expect(page.getByTestId("sandbox-take-win-1")).toBeDisabled();
  // an action reaches the sandbox; a second one is spaced by the page, not refused
  await page.getByTestId("win-action-notepad").click();
  await expect.poll(async () => (await record()).actions.map((a) => [a.action, a.sandbox, a.session])).toEqual([["notepad", "win-3", session]]);
  await page.getByTestId("win-action-start_menu").click();
  await expect.poll(async () => (await record()).actions.map((a) => a.action), { timeout: 5000 }).toEqual(["notepad", "start_menu"]);
  await expect(page.getByTestId("win-notice")).toHaveCount(0);
  // Reset restores the held sandbox and keeps the borrow
  await page.getByTestId("win-action-reset").click();
  await expect.poll(async () => (await mockWindows()).tiles[2]!.last_relaunch.by, { timeout: 5000 }).toBe(`reset:${session}`);
  await expect(page.getByTestId("win-action-notepad")).toBeDisabled();
  await expect(page.getByTestId("win-action-notepad")).toBeEnabled({ timeout: 15_000 });
  expect((await mockWindows()).tiles[2]!.borrowed!.session).toBe(session);
  // give back ends it: the panel goes, the sandbox is restored for the next viewer
  await page.getByTestId("win-give-back").click();
  await expect(panel).toHaveCount(0);
  await expect.poll(async () => (await mockWindows()).tiles[2]!.borrowed).toBeNull();
  expect((await mockWindows()).tiles[2]!.last_relaunch.by).toBe(`return:${session}`);
  await expect(page.getByTestId("sandbox-take-win-1")).toBeEnabled();
  await expect(hit(page, "win-3")).toHaveAttribute("data-phase", "live", { timeout: 15_000 });
});

test("a sandbox lent to another viewer shows no panel and cannot be reset or taken; giving it back returns it", async ({ page, browser }) => {
  await openWindows(page);
  const other = await viewer(browser);
  await page.getByTestId("sandbox-take-win-4").click();
  await expect(page.getByTestId("win-panel")).toBeVisible();
  await expect(other.getByTestId("win-panel")).toHaveCount(0);
  await expect(other.getByTestId("sandbox-lent-win-4")).toContainText(/^Lent to another viewer until \d\d:\d\d:\d\d UTC$/);
  await expect(other.getByTestId("sandbox-take-win-4")).toBeDisabled();
  await expect(other.getByTestId("sandbox-hit-win-4")).toBeDisabled();
  await expect(other.getByTestId("sandbox-take-win-1")).toBeEnabled();
  await page.getByTestId("win-give-back").click();
  await expect(other.getByTestId("sandbox-lent-win-4")).toHaveCount(0);
  await expect(other.getByTestId("sandbox-take-win-4")).toBeEnabled({ timeout: 15_000 });
  await other.context().close();
});

test("at the cap the third borrow is refused with the coordinator's reason", async ({ page, browser }) => {
  await openWindows(page);
  const a = await viewer(browser);
  const b = await viewer(browser);
  await a.getByTestId("sandbox-take-win-1").click();
  await b.getByTestId("sandbox-take-win-2").click();
  await expect(a.getByTestId("win-panel")).toBeVisible();
  await expect(b.getByTestId("win-panel")).toBeVisible();
  await page.getByTestId("sandbox-take-win-3").click();
  await expect(page.getByTestId("win-notice")).toHaveText(/^2 of 2 sandboxes are already lent; the next comes back in \d+ s$/);
  await a.context().close();
  await b.context().close();
});

test("an image from 150 days old shows its warning, and from 170 days the refusal", async ({ page }) => {
  await openWindows(page);
  await admin("/mock/windows", { image_age_days: 151 });
  await expect(page.getByTestId("win-warning")).toContainText("151 days old");
  await expect(page.getByTestId("win-refused")).toHaveCount(0);
  await admin("/mock/windows", { image_age_days: 171 });
  await expect(page.getByTestId("win-refused")).toContainText("171 days old");
  await admin("/mock/windows", { image_age_days: 12 });
  await expect(page.getByTestId("win-warning")).toHaveCount(0);
  await expect(page.getByTestId("win-refused")).toHaveCount(0);
});

test("nothing on the page sends an intent but the four, and none carries a point, a key or text", async ({ page }) => {
  const sent: { name: string; args: Record<string, unknown> }[] = [];
  page.on("websocket", (ws) =>
    ws.on("framesent", (f) => {
      if (typeof f.payload !== "string") return;
      const m = JSON.parse(f.payload) as { type?: string; name?: string; args?: Record<string, unknown> };
      if (m.type === "intent") sent.push({ name: m.name!, args: m.args ?? {} });
    }),
  );
  await openWindows(page);
  await hit(page, "win-1").click();
  await page.getByTestId("sandbox-take-win-3").click();
  await expect(page.getByTestId("win-panel")).toBeVisible();
  // clicks on the held desktop, on its panel and typing reach nothing
  const box = (await view(page)).cards.find((c) => c.slot === "win-3")!.box!;
  await page.mouse.click(box.x + box.w / 2, box.y + box.h / 2);
  await page.mouse.dblclick(box.x + 10, box.y + 10);
  await page.keyboard.type("calc.exe");
  await page.keyboard.press("Enter");
  await page.getByTestId("win-action-about").click();
  await expect.poll(async () => (await record()).actions.map((a) => a.action)).toEqual(["about"]);
  await page.getByTestId("win-give-back").click();
  await expect(page.getByTestId("win-panel")).toHaveCount(0);
  await page.waitForTimeout(500);
  expect(sent.map((s) => s.name)).toEqual(["windows_reset", "claim_windows", "windows_action", "release_windows"]);
  for (const s of sent) {
    expect(INTENTS).toContain(s.name);
    expect(Object.keys(s.args).every((k) => ["slot", "all", "action", "claim"].includes(k)), JSON.stringify(s)).toBe(true);
  }
  // and the mock received exactly those, none refused
  const got = (await record()).intents;
  expect(got.map((i) => [i.name, i.outcome])).toEqual([["windows_reset", "committed"], ["claim_windows", "committed"], ["windows_action", "committed"], ["release_windows", "committed"]]);
});

test("a viewer with no IPv6 whose IPv4 port does not answer sees each tile say so, with Retry, and the desktops once IPv4 answers", async ({ page }) => {
  await admin("/mock/fault", { ipv6_down: true, ipv4_silent: true });
  try {
    await page.goto("/?mode=windows&debug=1");
    await page.waitForFunction(() => window.__windows !== undefined);
    // While the page finds a route, and while a socket opens, the tile says so.
    await expect(page.getByTestId("sandbox-link-win-1")).toContainText(/Finding the way to the sandboxes\.|Connecting to win-1\./);
    // No socket of either family opens: the page settles on IPv4, each socket misses its open
    // deadline, and every tile says it cannot reach its desktop, when it tries again, and offers Retry.
    for (const slot of SLOTS) {
      await expect(page.getByTestId(`sandbox-link-${slot}`)).toContainText(`Cannot reach ${slot}'s desktop over IPv4.`, { timeout: 30_000 });
      await expect(page.getByTestId(`sandbox-link-${slot}`)).toContainText(/Trying again/);
      await expect(page.getByTestId(`sandbox-link-${slot}`)).toBeInViewport();
      await expect(page.getByTestId(`sandbox-retry-${slot}`)).toBeVisible();
      // The message covers the whole desktop, so the tile reads as one box, not a strip at its top.
      const msg = (await page.getByTestId(`sandbox-link-${slot}`).boundingBox())!;
      const tile = (await hit(page, slot).boundingBox())!;
      expect(Math.abs(msg.height - tile.height), `${slot}'s message height`).toBeLessThanOrEqual(2);
      expect(Math.abs(msg.width - tile.width), `${slot}'s message width`).toBeLessThanOrEqual(2);
      expect(await page.getByTestId(`sandbox-link-${slot}`).evaluate((e) => getComputedStyle(e).textDecorationLine)).toBe("none");
    }
    expect((await page.evaluate(() => window.__windows!.client.getState().net)).family).toBe("v4");
    // Retry does not reset the sandbox: the click lands on the button, not the tile.
    const resets = async () => (await record()).intents.filter((i) => i.name === "windows_reset").length;
    const before = await resets();
    await page.getByTestId("sandbox-retry-win-1").click();
    await page.waitForTimeout(500);
    expect(await resets()).toBe(before);
  } finally {
    await admin("/mock/fault", { ipv6_down: false, ipv4_silent: false });
  }
  // IPv4 answers now: Retry connects at once, and every desktop draws.
  await page.getByTestId("sandbox-retry-win-2").click().catch(() => {});
  await expect.poll(async () => page.evaluate(() => window.__windows!.client.bench?.tilesSynced ?? 0), { timeout: 45_000 }).toBeGreaterThanOrEqual(SANDBOXES);
  for (const slot of SLOTS) await expect(page.getByTestId(`sandbox-link-${slot}`)).toHaveCount(0);
});

test.describe("on a 1728 x 996 screen", () => {
  test.use({ viewport: { width: 1728, height: 996 } });

  test("the desktops fill the stage: two by two, with no empty band under them", async ({ page }) => {
    await openWindows(page);
    const v = await view(page);
    const stage = (await page.getByTestId("win-stage").boundingBox())!;
    const boxes = v.cards.map((c) => c.box!);
    expect(new Set(boxes.map((b) => b.y)).size).toBe(2);
    // One row of four was 411 px wide, with 500 px of empty page under the captions.
    for (const b of boxes) expect(b.w).toBeGreaterThan(600);
    const bottom = Math.max(...v.cards.map((c) => c.caption!.y + c.caption!.h));
    expect(stage.y + stage.height - bottom).toBeLessThan(40);
    for (const slot of SLOTS) await expect(hit(page, slot)).toBeInViewport({ ratio: 1 });
  });
});

test.describe("on a 390 px phone", () => {
  test.use({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true, deviceScaleFactor: 3 });

  /** Whether `inner` lies wholly inside `outer`, with a pixel's slack for rounding. */
  const inside = (inner: { x: number; y: number; width: number; height: number } | null, outer: { x: number; y: number; width: number; height: number } | null) =>
    inner !== null && outer !== null && inner.x >= outer.x - 1 && inner.y >= outer.y - 1 && inner.x + inner.width <= outer.x + outer.width + 1 && inner.y + inner.height <= outer.y + outer.height + 1;

  async function theLabelIsWhole(page: Page): Promise<void> {
    const label = page.getByTestId("win-label");
    await expect(label).toHaveText(LABEL);
    await expect(label).toBeInViewport({ ratio: 1 });
    expect(await label.evaluate((e) => e.scrollWidth <= e.clientWidth + 1 && e.scrollHeight <= e.clientHeight + 1)).toBe(true);
    expect(await shown(page, LABEL)).toBe(1);
  }

  test("the demonstration label is whole, once, with the panel open or closed, and Take over is never clipped", async ({ page }) => {
    await openWindows(page);
    await theLabelIsWhole(page);
    for (const slot of SLOTS) {
      const take = page.getByTestId(`sandbox-take-${slot}`);
      await expect(take).toBeVisible();
      const caption = await page.getByTestId(`sandbox-caption-${slot}`).boundingBox();
      expect(inside(await take.boundingBox(), caption), `${slot}'s Take over is clipped by its caption`).toBe(true);
      // the label inside the button is whole too
      expect(await take.evaluate((b) => b.scrollWidth <= b.clientWidth + 1)).toBe(true);
    }
    await page.getByTestId("sandbox-take-win-2").click();
    await expect(page.getByTestId("win-panel")).toBeVisible();
    await theLabelIsWhole(page);
    await page.getByTestId("win-give-back").click();
    await expect(page.getByTestId("win-panel")).toHaveCount(0);
  });
});
