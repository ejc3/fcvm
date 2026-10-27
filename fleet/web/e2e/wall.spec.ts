import { expect, test, devices, type Page } from "@playwright/test";
import { seconds, thousands, utcDate } from "../core/format.ts";
import { DETAIL_TILE_PX, layoutMosaic, streamHeightFor, zoomTarget } from "../core/layout.ts";
import { ZOOM_DIM_NEAR, ZOOM_SHEET_W, zoomInsetsFor } from "../core/view.ts";
import { streamBitrate } from "../core/bandwidth.ts";
import { COORD, admin, fleetNow, focusError, mockState, openWall, raceOver, seamReport, setSkew, setTimeOfDay } from "./helpers.ts";

const view = (page: Page) =>
  page.evaluate(() => {
    const v = window.__fleet!.view();
    return { version: v.version, tiles: v.tiles.map((t) => ({ id: t.id, name: t.name, half: t.half, box: t.box, dark: t.dark })), image: v.layout?.image ?? null };
  });

/** A tile's accessible name: its state in words, whether or not the tile is large enough to show them. */
const says = (page: Page, name: string) => page.locator(`.key-grid button[aria-label^='${name},']`);

const tileBox = async (page: Page, id: string) => (await view(page)).tiles.find((t) => t.id === id)!.box!;

// Mean absolute error, 0..255 per channel, at smooth points either side of each
// boundary, after one colour transform fitted over all of them (browsers convert
// YUV to RGB differently: before the fit, correct tiles read 1.0 in Chromium on
// arm64, 2.7 to 3.1 in WebKit and up to 5.1 in Chromium on CI's x86 runner).
// After it, correct draws at the common t measured 0.9 to 1.5 at worst over 16
// tiles in Chromium and WebKit; a tile drawn 0.6 s off its stamp 10.
const TILE_ERR_MAX = 3;
const ACROSS_ERR_MAX = 3;

/** The t drawn now: a capture forces a draw and reports its t. */
const drawnT = (page: Page) => page.evaluate(async () => (await window.__fleet!.client.capture([])).T);

test.describe.configure({ mode: "serial" });

test("the mosaic draws every GPU live, 15 tiles at one common t, each side as wide as its GPUs", { tag: "@engines" }, async ({ page }) => {
  await openWall(page, 15);
  const v = await view(page);
  expect(v.tiles).toHaveLength(15);
  expect(v.tiles.filter((t) => t.box !== null)).toHaveLength(15);
  const bench = await page.evaluate(() => window.__fleet!.client.bench!);
  expect(bench.tilesSynced).toBe(15);
  expect(bench.tilesBehind).toBe(0);
  // Eight fcvm GPUs against seven EC2: the line between them at 8/15 of the image.
  const line = await page.evaluate(() => window.__fleet!.view().layout!);
  expect((line.halfLineX! - line.image.x) / line.image.w).toBeCloseTo(8 / 15, 2);
  await expect(page.locator(".dark-tile")).toHaveCount(0);
});

test("the render is the page: the image fills the screen, the controls sit over it and fade when the pointer rests", async ({ page }) => {
  await openWall(page, 15);
  const vp = page.viewportSize()!;
  const L = await page.evaluate(() => window.__fleet!.view().layout!);
  // Contain-fit to the whole viewport, centred: no band reserved under it.
  const fill = Math.min(vp.width / L.image.w, vp.height / L.image.h);
  expect(fill).toBeCloseTo(1, 2);
  expect(Math.abs(L.image.x + L.image.w / 2 - vp.width / 2)).toBeLessThanOrEqual(1);
  expect(Math.abs(L.image.y + L.image.h / 2 - vp.height / 2)).toBeLessThanOrEqual(1);
  // What used to stand between the title and the image is gone from the main view.
  await expect(page.getByTestId("sentence")).toHaveCount(0);
  await expect(page.getByTestId("counter")).toHaveCount(0);
  await expect(page.getByTestId("feed")).toHaveCount(0);
  await expect(page.getByTestId("race-clock-fcvm")).toHaveCount(0);
  await expect(page.getByTestId("play")).toHaveText("Direct the scene");
  // Each half's tag on the image, either side of the divider, which sits exactly on halfLineX.
  const line = await page.getByTestId("half-line").boundingBox();
  expect(line!.x + line!.width / 2).toBeCloseTo(L.halfLineX!, 3);
  const f = await page.getByTestId("side-fcvm").boundingBox();
  const e = await page.getByTestId("side-ec2").boundingBox();
  await expect(page.getByTestId("side-fcvm")).toHaveText("FCVM · restored from a snapshot");
  await expect(page.getByTestId("side-ec2")).toHaveText("EC2 · launched from an AMI");
  expect(f!.x + f!.width).toBeLessThanOrEqual(L.halfLineX!);
  expect(e!.x).toBeGreaterThanOrEqual(L.halfLineX!);
  for (const b of [f!, e!]) {
    expect(b.y).toBeGreaterThanOrEqual(L.image.y);
    expect(b.y + b.height).toBeLessThan(L.image.y + L.image.h / 3);
  }
  // The spec line, from live numbers: the image in device pixels, the GPUs, the frame rate.
  await expect(page.getByTestId("spec")).toHaveText(new RegExp(`^${Math.round(L.image.w)}×${Math.round(L.image.h)} · 15 × .+ · \\d+ fps · path traced$`));
  await expect(page.getByTestId("live")).toHaveText("LIVE");
  // The first time the pointer is over the image, a hint says what a click does; once per browser.
  const t = (await view(page)).tiles.find((x) => x.id === "fcvm:3")!.box!;
  await page.mouse.move(t.x + t.w / 2, t.y + t.h / 2);
  await expect(page.getByTestId("hint")).toHaveText("Click a square to look at its VM.");
  await expect(page.getByTestId("hint")).toHaveCount(0, { timeout: 8000 });
  // At rest the controls fade, but not the title; any pointer move brings them back.
  await expect(page.locator(".wall")).toHaveClass(/\bidle\b/, { timeout: 6000 });
  expect(await page.locator(".hud-top .actions").evaluate((el) => getComputedStyle(el).opacity)).toBe("0");
  expect(await page.locator(".brand").evaluate((el) => getComputedStyle(el).opacity)).toBe("1");
  // Resting over the image, the seams and the outline have gone too.
  await expect(page.getByTestId("seams")).toHaveAttribute("data-visible", "0");
  await expect(page.getByTestId("hover-outline")).toHaveCount(0);
  await page.mouse.move(t.x + t.w / 2 + 5, t.y + t.h / 2);
  await expect(page.locator(".wall")).not.toHaveClass(/\bidle\b/);
  await expect(page.getByTestId("seams")).toHaveAttribute("data-visible", "1");
  await page.reload();
  await page.waitForFunction(() => window.__fleet !== undefined && window.__fleet.view().layout !== null);
  await page.mouse.move(t.x + t.w / 2, t.y + t.h / 2 + 3);
  await page.waitForTimeout(500);
  await expect(page.getByTestId("hint")).toHaveCount(0);
});

test("every control stays on screen from just above the phone breakpoint up", async ({ page }) => {
  await openWall(page, 15);
  // The mock lends a GPU, so "Get your own GPU" is in the row too.
  await expect(page.getByTestId("own-gpu")).toBeVisible();
  for (const width of [701, 760, 860, 1024, 1280]) {
    await page.setViewportSize({ width, height: 700 });
    await page.waitForTimeout(150);
    const off = await page.evaluate(() =>
      [...document.querySelectorAll(".hud-top button, .hud-bottom button")]
        .map((el) => ({ name: el.getAttribute("aria-label") ?? el.textContent?.trim() ?? "", r: el.getBoundingClientRect() }))
        .filter((x) => x.r.width > 0 && (x.r.left < 0 || x.r.right > window.innerWidth || x.r.top < 0 || x.r.bottom > window.innerHeight))
        .map((x) => `${x.name} ${Math.round(x.r.left)}..${Math.round(x.r.right)}`),
    );
    expect(off, `at ${width} px`).toEqual([]);
  }
});

test("no seam: pixels on both sides of every boundary match the picture at the drawn t", { tag: "@engines" }, async ({ page }) => {
  await openWall(page, 15);
  const r = await seamReport(page);
  console.log(`seams: T=${r.T} samples=${r.samples} worst tile ${Math.max(...Object.values(r.perTile)).toFixed(2)} worst boundary ${Math.max(...r.boundaries.map((b) => b.err)).toFixed(2)}`);
  // Smooth sample pairs only: 100 to 175 at 1024x640, more on larger screens, fewer where the camera holds on detail.
  expect(r.samples).toBeGreaterThanOrEqual(60);
  for (const [id, s] of Object.entries(r.shown)) if (s !== "dark") expect(s, id).toBe(`sync@${r.T}`);
  for (const [id, e] of Object.entries(r.perTile)) expect(e, `tile ${id}`).toBeLessThan(TILE_ERR_MAX);
  for (const b of r.boundaries) expect(b.err, `${b.a} | ${b.b}`).toBeLessThan(ACROSS_ERR_MAX);
});

test("the seam check catches a renderer that draws another t than it stamps", async ({ page }) => {
  await openWall(page, 15);
  await setSkew("slot-2", 0.6);
  try {
    // How much a 0.6 s skew changes the smooth pixels at fcvm:2's edges rises
    // and falls with t as the nebula moves (3.8 to 18 over ten runs, 1.9 once),
    // so measure while it shows: within 20 s the picture moves enough.
    let r = await seamReport(page);
    await expect.poll(async () => (r = await seamReport(page)).perTile["fcvm:2"] ?? 0, { timeout: 20_000, intervals: [500] }).toBeGreaterThan(TILE_ERR_MAX);
    console.log(`skewed: fcvm:2 ${r.perTile["fcvm:2"]?.toFixed(2)}, others worst ${Math.max(...Object.entries(r.perTile).filter(([id]) => id !== "fcvm:2").map(([, e]) => e)).toFixed(2)}`);
    expect(r.perTile["fcvm:2"]!).toBeGreaterThan(TILE_ERR_MAX);
    for (const [id, e] of Object.entries(r.perTile)) if (id !== "fcvm:2") expect(e, id).toBeLessThan(TILE_ERR_MAX);
    const touching = r.boundaries.filter((b) => b.a === "fcvm:2" || b.b === "fcvm:2");
    expect(touching.length).toBeGreaterThan(0);
    expect(Math.max(...touching.map((b) => b.err))).toBeGreaterThan(ACROSS_ERR_MAX);
  } finally {
    await setSkew("slot-2", 0);
  }
});

test("restart all: the race is on the picture, a clock per half and per tile, fcvm freezes while EC2 counts, results announced", async ({ page, browser }) => {
  await openWall(page, 15);
  const watcher = await (await browser.newContext()).newPage();
  await openWall(watcher, 15);
  await expect(page.getByTestId("restart-all")).toHaveText("Restart all 15 GPUs");
  await expect(page.getByTestId("announce")).toHaveAttribute("aria-live", "polite");
  await page.getByTestId("restart-all").click();
  // Who started it, briefly, on the image.
  await expect(page.getByTestId("toast")).toHaveText("you restarted every GPU");
  await expect(watcher.getByTestId("toast")).toHaveText(/viewer \d+ restarted every GPU/);
  await expect(page.getByTestId("restart-all")).toBeDisabled();
  await expect(page.getByTestId("restart-all")).toHaveText("Restarting now");
  await expect(page.getByTestId("countdown")).toContainText("Restart running");
  await expect(watcher.getByTestId("countdown")).toContainText("Restart running");
  // Every dark tile counts, with how far its restart is; each half has one big clock.
  await expect(page.locator('[data-testid^="dark-fcvm"] svg.glyph.progress').first()).toBeVisible();
  await expect(page.getByTestId("clock-ec2:1")).toHaveText(/^0:\d\d\.\d$/);
  await expect(page.getByTestId("race-clock-fcvm")).toBeVisible();
  await expect(page.getByTestId("race-clock-ec2")).toHaveAttribute("data-state", "counting");
  // fcvm comes back: its clock freezes, while EC2's runs on, and each relit tile shows its own time.
  await expect(page.getByTestId("race-clock-fcvm")).toHaveAttribute("data-state", "done", { timeout: 10_000 });
  await expect(page.getByTestId("race-sub-fcvm")).toHaveText("all 8 GPUs back");
  await expect(page.getByTestId("chip-fcvm:1")).toHaveText(/^\d\.\d s$/);
  await expect(page.getByTestId("race-sub-ec2")).toHaveText(/^\+\d+ s and counting$/);
  await expect(page.getByTestId("race-steps-ec2")).toContainText("instance");
  const a = { f: await page.getByTestId("race-value-fcvm").textContent(), e: await page.getByTestId("race-value-ec2").textContent() };
  await page.waitForTimeout(1200);
  const b = { f: await page.getByTestId("race-value-fcvm").textContent(), e: await page.getByTestId("race-value-ec2").textContent() };
  expect(a.f).toMatch(/^\d\.\d s$/);
  expect(b.f).toBe(a.f);
  expect(b.e).not.toBe(a.e);
  await expect(page.getByTestId("announce")).toHaveText(/^fcvm: all 8 GPUs streaming again \d\.\d s after the restart\.$/);
  // EC2 comes back; both results are announced, and one card says the race in the coordinator's numbers.
  await expect(page.getByTestId("announce")).toContainText(/EC2: all 7 GPUs streaming again \d+ s after the restart\./, { timeout: 30_000 });
  const st = await fleetNow();
  const ra = st.relaunch_all!;
  const f = ra.fcvm_done_at! - ra.started_at;
  const e = ra.ec2_done_at! - ra.started_at - ra.ec2_capacity_wait_ms;
  expect(f).toBeLessThan(e);
  const ratio = (x: number, y: number) => (y / x >= 10 ? Math.round(y / x) : (y / x).toFixed(1));
  await expect(page.getByTestId("end-card")).toHaveText(`fcvm back in ${seconds(f)} · EC2 back in ${seconds(e)} · ${ratio(f, e)}× faster`);
  await expect(page.getByTestId("race-clock-fcvm")).toHaveCount(0);
  await expect(page.getByTestId("restart-all")).toBeEnabled();
  // The card stays a few seconds, then the strip's one line says how the race went.
  await expect(page.getByTestId("end-card")).toHaveCount(0, { timeout: 15_000 });
  await expect(page.getByTestId("last-race")).toHaveText(
    `Last race: fcvm back in ${seconds(st.scoreboard.fcvm!.last_ms)} · EC2 back in ${seconds(st.scoreboard.ec2!.last_ms)} · ${ratio(st.scoreboard.fcvm!.last_ms, st.scoreboard.ec2!.last_ms)}× faster`,
  );
  await expect(page.getByTestId("toast")).toHaveCount(0);
  await watcher.context().close();
});

test("the owner's schedule: a shared countdown, a restart by the schedule, a cooldown for everyone, and a capacity wait drawn hatched and not counted", async ({ page, browser }) => {
  await openWall(page, 15);
  const other = await (await browser.newContext()).newPage();
  await openWall(other, 15);
  await admin("/mock/schedule", { next_in_ms: 4000, cooldown_s: 40, capacity_wait_ms: 3000 });
  await expect(page.getByTestId("countdown")).toContainText("auto in");
  await expect(page.getByTestId("countdown-value")).toHaveText(/^0:0[1-4]$/);
  await expect(other.getByTestId("countdown-value")).toHaveText(/^0:0[1-4]$/);
  await expect(page.getByTestId("countdown")).toContainText("Scheduled restart, running", { timeout: 8000 });
  await expect(page.getByTestId("toast")).toHaveText("the scheduled restart of every GPU began");
  // While AWS has no spot GPU: EC2's clock says it is not counting the wait, and the tiles say so, hatched.
  await expect(page.getByTestId("race-hatch-ec2")).toHaveText(/^not counting \d s AWS spent finding a spot GPU$/);
  await expect(page.getByTestId("dark-ec2:1")).toHaveAttribute("data-kind", "capacity");
  await expect(page.locator(".key-grid button[aria-label^='EC2 VM 1,']")).toHaveAttribute("aria-label", /AWS had no spot GPU/);
  await expect(page.getByTestId("dark-ec2:1")).toHaveClass(/hatched/);
  await raceOver();
  const st = await fleetNow();
  expect(st.relaunch_all!.by).toBe("schedule");
  expect(st.scoreboard.ec2!.last_capacity_wait_ms).toBe(3000);
  // The headline leaves the wait out: EC2's last time is click to last frame less the wait.
  expect(st.scoreboard.ec2!.last_ms).toBe(st.relaunch_all!.ec2_done_at! - st.relaunch_all!.started_at - 3000);
  await expect(page.getByTestId("end-card")).toContainText(`EC2 back in ${seconds(st.scoreboard.ec2!.last_ms)}`);
  // The cooldown, shown to everyone: the button waits.
  await expect(other.getByTestId("restart-all")).toBeDisabled();
  await expect(other.getByTestId("restart-note")).toHaveText(/^available again in 0:[0-3]\d$/);
  await admin("/mock/schedule", { cooldown_s: 0, clear_cooldown: true });
  await expect(other.getByTestId("restart-all")).toBeEnabled();
  await other.context().close();
});

/**
 * Re-tile the mock to these GPU counts and record every draw until the page has
 * swapped to the new generation and every tile is live again.
 */
async function retile(page: Page, fcvm: number, ec2: number, live: number) {
  const gen0 = await page.evaluate(() => window.__fleet!.client.getState().accepted!.layout.generation);
  await page.evaluate(() => window.__fleet!.client.traceStart());
  await admin("/mock/fault", { fcvm_gpus: fcvm, ec2_gpus: ec2 });
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.getState().shownLayout?.gen), { timeout: 15_000 }).toBe(gen0 + 1);
  const atT = await page.evaluate(() => window.__fleet!.client.getState().accepted!.layout.from_frame / 30);
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.bench?.tilesSynced ?? 0), { timeout: 30_000 }).toBe(live);
  const trace = await page.evaluate(() => window.__fleet!.client.traceStop());
  return { gen0, atT, trace };
}

/** No draw without a frame, t never going back, the old tiling until frames at its first frame, then the new one. */
function expectNoHitch(r: Awaited<ReturnType<typeof retile>>): void {
  const { gen0, atT, trace } = r;
  // Around the swap no region is ever drawn without a frame. (A GPU that joined
  // starts its VM first, about 3 s, and fades in when its stream starts.)
  expect(trace.filter((e) => e.missing > 0 && e.T! < atT + 1.5)).toEqual([]);
  for (let i = 1; i < trace.length; i++) expect(trace[i]!.T!, `draw ${i}`).toBeGreaterThanOrEqual(trace[i - 1]!.T!);
  const first = trace.findIndex((e) => e.gen === gen0 + 1);
  expect(first).toBeGreaterThan(0);
  expect(trace.slice(0, first).every((e) => e.gen === gen0 && e.T! < atT)).toBe(true);
  expect(trace.slice(first).every((e) => e.gen === gen0 + 1 && e.T! >= atT - 1e-6)).toBe(true);
}

test("GPUs join and leave on each side: the old tiles keep drawing until the new ones have frames at the re-tile's frame, then all swap at once", { tag: "@engines" }, async ({ page }) => {
  await openWall(page, 15);
  // Two join the fcvm side (a new host): the line moves right, every new tile live after its VM starts.
  let r = await retile(page, 10, 7, 17);
  expectNoHitch(r);
  let L = await page.evaluate(() => window.__fleet!.view().layout!);
  expect((L.halfLineX! - L.image.x) / L.image.w).toBeCloseTo(10 / 17, 2);
  expect(await page.evaluate(() => window.__fleet!.view().tiles.length)).toBe(17);
  // Three leave the EC2 side.
  r = await retile(page, 10, 4, 14);
  expectNoHitch(r);
  L = await page.evaluate(() => window.__fleet!.view().layout!);
  expect((L.halfLineX! - L.image.x) / L.image.w).toBeCloseTo(10 / 14, 2);
  // The hover seams follow the new tiling.
  const t = await tileBox(page, "ec2:2");
  await page.mouse.move(t.x + t.w / 2, t.y + t.h / 2);
  await expect(page.getByTestId("hover-outline")).toHaveAttribute("data-tile", "EC2 VM 2");
  const outline = await page.getByTestId("hover-outline").evaluate((el) => el.getBoundingClientRect().width);
  expect(outline).toBeCloseTo(t.w, 0);
  // Back to eight and seven.
  r = await retile(page, 8, 7, 15);
  expectNoHitch(r);
});

test("a re-tile the page hears of after its frame has passed still draws nothing in the wrong place", async ({ page }) => {
  // Hold back every snapshot of the new generation for 1.5 s, three times the
  // mock's half-second lead: frames of the new tiling arrive before the page knows.
  let late: number | null = null;
  await page.routeWebSocket(/\/live$/, (ws) => {
    const server = ws.connectToServer();
    server.onMessage((m) => {
      const text = String(m);
      if (late !== null && text.includes('"type":"snapshot"') && (JSON.parse(text) as { state: { layout: { generation: number } } }).state.layout.generation >= late) {
        setTimeout(() => ws.send(text), 1500);
        return;
      }
      ws.send(m);
    });
    ws.onMessage((m) => server.send(m));
  });
  await openWall(page, 15);
  late = (await page.evaluate(() => window.__fleet!.client.getState().accepted!.layout.generation)) + 1;
  try {
    const r = await retile(page, 10, 7, 17);
    expectNoHitch(r);
  } finally {
    late = null;
  }
  await retile(page, 8, 7, 15);
});

test("one side with no GPUs: the other takes the full width, and a small label says the side is empty", async ({ page }) => {
  await openWall(page, 15);
  try {
    const r = await retile(page, 8, 0, 8);
    expectNoHitch(r);
    const L = await page.evaluate(() => window.__fleet!.view().layout!);
    expect(L.halfLineX).toBeNull();
    const boxes = await page.evaluate(() => window.__fleet!.view().tiles.map((t) => t.box!));
    expect(Math.max(...boxes.map((b) => b.x + b.w))).toBeCloseTo(L.image.x + L.image.w, 0);
    await expect(page.getByTestId("empty-ec2")).toHaveText("EC2: no GPUs live right now");
    await expect(page.getByTestId("side-ec2")).toHaveCount(0);
    await expect(page.getByTestId("side-fcvm")).toHaveText("FCVM · restored from a snapshot");
    await expect(page.locator(".half-line")).toHaveCount(0);
  } finally {
    await retile(page, 8, 7, 15);
  }
  await expect(page.getByTestId("empty-ec2")).toHaveCount(0);
});

test("hovering the image fades in every seam, outlines the tile and labels it, off every label", { tag: "@engines" }, async ({ page }) => {
  await openWall(page, 15);
  const v = await view(page);
  await expect(page.getByTestId("seams")).toHaveAttribute("data-visible", "0");
  const t = v.tiles.find((x) => x.id === "ec2:3")!;
  await page.mouse.move(t.box!.x + t.box!.w / 2, t.box!.y + t.box!.h / 2);
  await expect(page.getByTestId("seams")).toHaveAttribute("data-visible", "1");
  await expect(page.getByTestId("hover-outline")).toHaveAttribute("data-tile", "EC2 VM 3");
  const outline = await page.getByTestId("hover-outline").evaluate((el) => {
    const r = el.getBoundingClientRect();
    return { x: r.x, y: r.y, w: r.width, h: r.height, color: getComputedStyle(el).borderTopColor };
  });
  expect(outline).toMatchObject({ x: t.box!.x, y: t.box!.y, w: t.box!.w, h: t.box!.h, color: "rgb(245, 165, 36)" });
  const label = page.getByTestId("hover-label");
  await expect(label).toContainText("EC2 VM 3");
  await expect(label).toContainText("Tesla T4");
  // The public place, never the host or instance name (#1366).
  await expect(label).toContainText("EC2 in mock-region");
  await expect(label).not.toContainText("mock-i-");
  await expect(label).toContainText(/launch time not recorded|last restart \d/);
  // Under the seam's own labels: the hover label covers none of them.
  const top = v.tiles.find((x) => x.id === "fcvm:2")!;
  await page.mouse.move(top.box!.x + top.box!.w / 2, top.box!.y + 12);
  await expect(page.getByTestId("hover-outline")).toHaveAttribute("data-tile", "fcvm VM 2");
  const covered = await page.evaluate(() => {
    const l = document.querySelector(".hover-label")!.getBoundingClientRect();
    return [...document.querySelectorAll("[data-avoid]")].some((el) => {
      const c = el.getBoundingClientRect();
      return l.left < c.right && c.left < l.right && l.top < c.bottom && c.top < l.bottom;
    });
  });
  expect(covered).toBe(false);
  expect(await page.getByTestId("seams").locator("line").count()).toBeGreaterThanOrEqual(10);
  // Off the image, onto the page around it.
  await page.mouse.move(5, page.viewportSize()!.height - 5);
  await expect(page.getByTestId("seams")).toHaveAttribute("data-visible", "0");
  await expect(page.getByTestId("hover-outline")).toHaveCount(0);
});

test("keyboard: the tile grid takes focus, arrows move the outline, Enter opens, Escape returns", async ({ page }) => {
  await openWall(page, 15);
  await page.locator('.key-grid button[tabindex="0"]').focus();
  await expect(page.getByTestId("hover-outline")).toHaveAttribute("data-tile", "fcvm VM 1");
  await expect(page.getByTestId("seams")).toHaveAttribute("data-visible", "1");
  await page.keyboard.press("ArrowRight");
  await expect(page.getByTestId("hover-outline")).toHaveAttribute("data-tile", "fcvm VM 2");
  await page.keyboard.press("ArrowRight");
  await expect(page.getByTestId("hover-outline")).toHaveAttribute("data-tile", "EC2 VM 1");
  await page.keyboard.press("ArrowDown");
  await expect(page.getByTestId("hover-outline")).toHaveAttribute("data-tile", "EC2 VM 3");
  await page.keyboard.press("Enter");
  await expect(page.getByTestId("focus")).toBeVisible();
  await expect(page.getByRole("heading", { name: "EC2 VM 3" })).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("focus")).toHaveCount(0);
  await expect(page.locator(".key-grid button:focus")).toHaveAttribute("aria-label", /^EC2 VM 3,/);
});

test("an opened VM zooms to fill the screen at its detail height, with the tiles around it dimmed, its timeline and live GPU readings", async ({ page }) => {
  await openWall(page, 15);
  const t = await tileBox(page, "fcvm:3");
  await page.mouse.click(t.x + t.w / 2, t.y + t.h / 2);
  await expect(page.getByTestId("focus")).toBeVisible();
  await expect.poll(() => page.evaluate(() => window.__fleet!.view().zoom?.dim ?? 0)).toBe(1);
  const z = await page.evaluate(() => window.__fleet!.view().plan.zoom!);
  const vp = page.viewportSize()!;
  const r = await page.evaluate(() => window.__fleet!.client.getState().accepted!.tiles.find((t) => t.id === "fcvm:3")!.rect);
  const aspect = (r.w * 16) / 9 / r.h;
  // It fills the room the bar, the margins and (when it costs no size) the sheet leave.
  const target = zoomTarget({ x: 0, y: 0, w: vp.width, h: vp.height }, r, 16 / 9, zoomInsetsFor(false, ZOOM_SHEET_W + 16));
  expect(Math.abs(z.box!.w - target.w)).toBeLessThanOrEqual(1);
  expect(Math.abs(z.box!.h - target.h)).toBeLessThanOrEqual(1);
  expect(z.box!.w / z.box!.h).toBeCloseTo(aspect, 1);
  // Its stream: the detail height that covers the box at 1x, so the picture is not scaled up.
  const wanted = z.source!.height;
  expect(wanted).toBe(streamHeightFor(z.box!.h, aspect, DETAIL_TILE_PX));
  expect(wanted).toBeGreaterThanOrEqual(z.box!.h);
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.getState().frames!.tiles["d:fcvm:3"]!.height), { timeout: 15_000 }).toBe(wanted);
  await expect(page.getByTestId("focus-stream")).toHaveText(new RegExp(`^${wanted}p · \\d+ fps · (native size|shown at \\d+%)$`));
  // The tiles still on screen keep streaming, at q=lo; the ones the zoom pushed off screen ask for the smallest.
  const others = await page.evaluate(() => window.__fleet!.view().plan.direct.filter((x) => x.id !== "d:fcvm:3").map((x) => ({ shown: x.box !== null, h: x.height, url: x.url })));
  expect(others.some((o) => o.shown)).toBe(true);
  for (const o of others) {
    if (o.shown) expect(o.url).toContain("q=lo");
    else expect(o.h).toBe(72);
  }
  await expect(page.getByTestId("readings")).toContainText("Temperature");
  await expect(page.getByTestId("readings")).toContainText(/\d+ °C/);
  await expect(page.getByTestId("readings")).toContainText(/\d+ W/);
  // The restart before this test left a timeline measured by the coordinator.
  await expect(page.getByTestId("timeline")).toContainText("GPU reset and driver loaded");
  await expect(page.getByTestId("timeline-total")).toHaveText(/^\d\.\d s$/);
  await page.waitForTimeout(1000);
  const fe = await focusError(page, "fcvm:3");
  console.log(`opened VM: error ${fe.err.toFixed(2)} shown ${fe.shown} brightness around it ${fe.around.toFixed(2)}`);
  expect(fe.shown).toBe(`sync@${fe.T}`);
  expect(fe.err).toBeLessThan(6);
  // Dimmed: at most 1 - ZOOM_DIM_NEAR of the brightness the same scene has in the opened tile.
  expect(fe.around).toBeLessThan(1 - ZOOM_DIM_NEAR + 0.08);
  expect(fe.around).toBeGreaterThan(0.05);
  // The sheet starts open; it folds away and comes back, and never moves the picture.
  const sheet = page.getByTestId("focus-sheet");
  await expect(sheet).toHaveClass(/open/);
  await page.getByTestId("sheet-hide").click();
  await expect(sheet).not.toHaveClass(/open/);
  expect(await page.evaluate(() => window.__fleet!.view().plan.zoom!.box)).toEqual(z.box);
  await page.getByTestId("sheet-toggle").click();
  await expect(sheet).toHaveClass(/open/);
  // A click on a dimmed tile opens it instead: the middle of the first one with a clear patch on
  // screen, below the bar and left of the sheet.
  const pick = await page.evaluate(({ w, h }) => {
    const v = window.__fleet!.view();
    for (const d of v.plan.direct) {
      if (!d.box) continue;
      const x0 = Math.max(d.box.x, 0) + 8;
      const x1 = Math.min(d.box.x + d.box.w, w - 420) - 8;
      const y0 = Math.max(d.box.y, 80) + 8;
      const y1 = Math.min(d.box.y + d.box.h, h) - 8;
      if (x1 > x0 && y1 > y0) return { id: d.id.slice(2), x: (x0 + x1) / 2, y: (y0 + y1) / 2 };
    }
    return null;
  }, { w: vp.width, h: vp.height });
  expect(pick).not.toBeNull();
  await page.mouse.click(pick!.x, pick!.y);
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.getState().ui.focusId)).toBe(pick!.id);
  await expect(page.locator("#focus-title")).not.toHaveText("fcvm VM 3");
  await page.getByRole("button", { name: "Back to the wall" }).click();
  await expect(page.getByTestId("focus")).toHaveCount(0);
  await expect.poll(() => page.evaluate(() => window.__fleet!.view().zoom)).toBeNull();
});

test("an opened VM shows its details, GPU graph and restart timeline on screen without a click, on a desktop and on a phone", async ({ page, browser }) => {
  // `fits`: the whole sheet is on screen without scrolling (a laptop-sized window). A phone's sheet scrolls.
  const check = async (p: Page, fits: boolean) => {
    const t = await p.evaluate(() => window.__fleet!.view().tiles.find((x) => x.id === "fcvm:3")!.box!);
    await p.mouse.click(t.x + t.w / 2, t.y + t.h / 2);
    await expect(p.getByTestId("focus")).toBeVisible();
    await expect.poll(() => p.evaluate(() => window.__fleet!.view().zoom?.dim ?? 0)).toBe(1);
    const sheet = p.getByTestId("focus-sheet");
    await expect(sheet).toHaveClass(/open/);
    expect(await sheet.evaluate((el) => getComputedStyle(el).opacity)).toBe("1");
    expect(await sheet.evaluate((el) => el.hasAttribute("inert") || el.querySelector("[inert]") !== null)).toBe(false);
    for (const id of ["readings", "restart-one"]) await expect(p.getByTestId(id), id).toBeInViewport();
    if (!fits) await p.getByTestId("timeline-total").scrollIntoViewIfNeeded();
    await expect(p.getByTestId("timeline-total")).toBeInViewport();
    await expect(p.locator(".zoom .phase-bar")).toBeInViewport();
    await expect(p.locator(".zoom dl")).toContainText("GPU");
    await expect(p.locator(".zoom dl")).toContainText("Up for");
    // The GPU graph: a sparkline per reading once two samples have arrived.
    await expect.poll(() => p.locator('[data-testid="readings"] polyline').count(), { timeout: 15_000 }).toBeGreaterThan(0);
    await expect(p.locator('[data-testid="readings"] polyline').first()).toBeInViewport();
    // The picture is not under the sheet's top edge: the opened VM is still on screen beside or above it.
    const box = await p.evaluate(() => window.__fleet!.view().plan.zoom!.box!);
    const sb = (await sheet.boundingBox())!;
    expect(box.y).toBeLessThan(sb.y + sb.height);
  };
  await openWall(page, 15);
  await check(page, true);
  const ctx = await browser.newContext({ ...devices["iPhone 13"], deviceScaleFactor: 1 });
  const phone = await ctx.newPage();
  await openWall(phone, 3);
  await check(phone, false);
  // On a phone the sheet starts below the picture, so the whole opened VM stays in view.
  const box = await phone.evaluate(() => window.__fleet!.view().plan.zoom!.box!);
  const sb = (await phone.getByTestId("focus-sheet").boundingBox())!;
  expect(box.y + box.h).toBeLessThanOrEqual(sb.y + 1);
  await ctx.close();
});

test("a folded side sheet leaves the keyboard path, and focus goes back to its Details button", async ({ page }) => {
  await openWall(page, 15);
  const t = await tileBox(page, "fcvm:3");
  await page.mouse.click(t.x + t.w / 2, t.y + t.h / 2);
  await expect(page.getByTestId("focus")).toBeVisible();
  const sheet = page.getByTestId("focus-sheet");
  if (!(await sheet.getAttribute("class"))!.includes("open")) await page.getByTestId("sheet-toggle").click();
  await expect(sheet).toHaveClass(/open/);
  await page.getByTestId("sheet-hide").focus();
  await page.keyboard.press("Enter");
  await expect(sheet).not.toHaveClass(/open/);
  await expect(page.getByTestId("sheet-toggle")).toBeFocused();
  // Nothing in the folded sheet takes focus: Tab from the Details button leaves the dialog's visible controls.
  for (let i = 0; i < 6; i++) {
    await page.keyboard.press("Tab");
    expect(await page.evaluate(() => document.activeElement?.closest("#focus-sheet") !== null)).toBe(false);
  }
});

test("a repeated intent id applies once, and both receipts are the same", async ({ page }) => {
  await openWall(page, 15);
  await page.getByTestId("play").click();
  const id = await page.evaluate(() => {
    const id = crypto.randomUUID();
    window.__fleet!.client.send("word", { text: "twice" }, id);
    window.__fleet!.client.resend(id);
    return id;
  });
  await expect(page.getByTestId("word-status")).toHaveText("“twice” is in the world");
  const applied = (await (await fetch(`${COORD}/mock/applied?id=${id}`)).json()) as { count: number };
  expect(applied.count).toBe(1);
  const st = await mockState();
  expect(st.state.scene.words.filter((w) => w.text === "twice")).toHaveLength(1);
});

test("the text check refuses a word, and the page says so", async ({ page }) => {
  await openWall(page, 15);
  await page.getByTestId("play").click();
  await page.getByLabel("Write a word into the world").fill("forbidden");
  await page.getByRole("button", { name: "Write" }).click();
  await expect(page.getByTestId("word-status")).toHaveText("Checking “forbidden”");
  await expect(page.getByTestId("word-status")).toHaveText("“forbidden” was not added: the text check refused it");
  const st = await mockState();
  expect(st.state.scene.words.some((w) => w.text === "forbidden")).toBe(false);
});

test("the sender picks how the message is drawn: Skywriting, with the keyboard", async ({ page }) => {
  await openWall(page, 15);
  await page.getByTestId("play").click();
  const lockup = page.getByRole("radio", { name: "Lockup" });
  await expect(lockup).toBeChecked();
  await expect(page.getByTestId("text-style-lockup").locator("img")).toHaveJSProperty("complete", true);
  expect(await page.getByTestId("text-style").locator("img").evaluateAll((imgs) => imgs.map((i) => (i as HTMLImageElement).naturalWidth))).toEqual([320, 320, 320]);
  // Arrow keys move between the three, as in any radio group.
  await lockup.focus();
  await page.keyboard.press("ArrowRight");
  await expect(page.getByRole("radio", { name: "Skywriting" })).toBeChecked();
  await page.getByLabel("Write a word into the world").fill("smoke");
  await page.keyboard.press("Enter");
  await expect(page.getByTestId("word-status")).toHaveText("“smoke” is in the world");
  const sent = await page.evaluate(() => window.__fleet!.client.getState().accepted!.actions.filter((a) => a.name === "word" && a.args?.["text"] === "smoke").at(-1)!.args);
  expect(sent).toEqual({ text: "smoke", text_style: "skywriting" });
  const st = (await mockState()).state as unknown as { scene: { words: { text: string; text_style: string }[] } };
  expect(st.scene.words.find((w) => w.text === "smoke")!.text_style).toBe("skywriting");
});

test("viewers choose the directed camera's next stop", async ({ page }) => {
  await openWall(page, 15);
  await page.getByTestId("play").click();
  await page.getByTestId("nudge-stars").click();
  await expect(page.getByTestId("nudge-stars")).toHaveAttribute("aria-pressed", "true");
  await expect(page.getByTestId("nudge-stars")).toContainText("1 vote");
  await admin("/mock/schedule", { hold_in_ms: 0 });
  await expect(page.getByTestId("nudge-note")).toContainText("holding on the star field");
  await expect(page.getByTestId("nudge-stars")).toContainText("holding now");
  expect((await fleetNow()).nudges.holding).toBe("stars");
  await admin("/mock/schedule", { hold_in_ms: 600_000 });
});

test("the count of GPU VMs restored ticks as VMs restore; the isolation panel says how a square is isolated", async ({ page }) => {
  await openWall(page, 15);
  const counters = async () => ((await mockState()).state as unknown as { counters: { restores: number; scrubs: number } }).counters;
  const n0 = (await counters()).restores;
  // The counters are in the drawer's About section, not on the image.
  await expect(page.getByTestId("counter")).toHaveCount(0);
  await page.getByTestId("menu").click();
  await expect(page.locator("#about-title")).toBeFocused();
  await expect(page.getByTestId("counter")).toContainText("GPU VMs restored:");
  await expect(page.getByTestId("counter-value")).toHaveText(thousands(n0));
  await expect(page.getByTestId("counter-scrubs")).toHaveCount(0);
  await page.evaluate(() => window.__fleet!.client.send("relaunch", { tile: "fcvm:1" }));
  await expect(page.getByTestId("counter-value")).toHaveText(thousands(n0 + 1), { timeout: 10_000 });
  // The new total arrives highlighted: it ticks.
  expect(await page.getByTestId("counter-value").evaluate((el) => getComputedStyle(el).animationName)).toBe("tick");
  await page.getByRole("button", { name: "Close" }).click();
  await page.getByTestId("isolation-link").click();
  await expect(page.getByTestId("drawer")).toBeVisible();
  await expect(page.locator("#isolation-title")).toBeFocused();
  const panel = page.getByTestId("isolation");
  await expect(panel.locator("p")).toHaveText([
    "Each square is its own virtual machine with its own Linux kernel.",
    "Its GPU is handed to that virtual machine alone by the hardware's IOMMU (VFIO passthrough), not shared through a proxy in the host kernel.",
    "A restart throws the old virtual machine away, restores a fresh one from a snapshot, and attaches the GPU as it restores.",
  ]);
  await expect(page.getByTestId("isolation-repo")).toHaveAttribute("href", "https://github.com/ejc3/fcvm");
  await expect(panel.locator("a")).toHaveCount(1);
  // Once an agent reports a scrub (`counters.scrubs`), the page says so, and counts them.
  expect((await counters()).scrubs).toBe(0);
  await admin("/mock/schedule", { gpu_scrub: true });
  try {
    await page.evaluate(() => window.__fleet!.client.send("relaunch", { tile: "fcvm:1" }));
    await expect(panel.locator("p").last()).toHaveText("GPU memory is cleared before a GPU moves to the next virtual machine.", { timeout: 10_000 });
    await expect(page.getByTestId("counter-scrubs")).toHaveText("GPU memory scrubs: 1");
  } finally {
    await admin("/mock/schedule", { gpu_scrub: false });
  }
});

test("a dropped light lands in the world where it was clicked", async ({ page }) => {
  await openWall(page, 15);
  // The directed camera holds still (the light is placed on the camera at the drawn t).
  await expect.poll(async () => {
    const p = (await mockState()).pattern;
    return Date.now() - p.epoch_unix_ms > (p.camera.at(-1)!.t + 0.5) * 1000;
  }, { timeout: 10_000 }).toBe(true);
  const before = (await mockState()).pattern.lights.length;
  await page.getByTestId("play").click();
  await page.getByTestId("light").click();
  // The drawer steps aside so the picture can be clicked.
  await expect(page.getByTestId("drawer")).toHaveCount(0);
  const img = await page.evaluate(() => window.__fleet!.view().layout!.image);
  const [u, v] = [0.3, 0.4];
  await page.mouse.click(img.x + u * img.w, img.y + v * img.h);
  await expect.poll(async () => (await mockState()).pattern.lights.length).toBe(before + 1);
  const st = await mockState();
  const sent = (st.state as unknown as { scene: { lights: { pos: number[] }[] } }).scene.lights.at(-1)!.pos;
  expect(sent).toHaveLength(3);
  const cam = st.pattern.camera.at(-1) as unknown as { x: number; y: number; zoom: number };
  const light = st.pattern.lights.at(-1)!;
  expect(light.pos.x).toBeCloseTo(((u - 0.5) * (16 / 9)) / cam.zoom + cam.x, 2);
  expect(light.pos.y).toBeCloseTo((v - 0.5) / cam.zoom + cam.y, 2);
});

test("two viewers hold the same state at the same version", async ({ browser }) => {
  const a = await browser.newContext();
  const b = await browser.newContext();
  const pa = await a.newPage();
  const pb = await b.newPage();
  await openWall(pa, 15);
  await openWall(pb, 15);
  await setTimeOfDay(0.75);
  await expect.poll(() => pb.evaluate(() => window.__fleet!.client.getState().accepted!.scene.time_of_day)).toBe(0.75);
  let same = false;
  for (let i = 0; i < 50 && !same; i++) {
    const [sa, sb] = await Promise.all([
      pa.evaluate(() => JSON.stringify([window.__fleet!.client.getState().version, window.__fleet!.client.getState().accepted])),
      pb.evaluate(() => JSON.stringify([window.__fleet!.client.getState().version, window.__fleet!.client.getState().accepted])),
    ]);
    if (JSON.parse(sa)[0] === JSON.parse(sb)[0]) {
      expect(sa).toBe(sb);
      same = true;
    } else {
      await pa.waitForTimeout(50);
    }
  }
  expect(same).toBe(true);
  await setTimeOfDay(0.42);
  await a.close();
  await b.close();
});

test("this viewer's own actions read as you", async ({ page }) => {
  await openWall(page, 15);
  await page.evaluate(() => window.__fleet!.client.send("light", { pos: [0, 1, -2], color: [1, 0.8, 0.5] }));
  await expect(page.getByTestId("toast")).toHaveText("you dropped a light");
  await page.getByTestId("menu").click();
  await expect(page.getByTestId("feed")).toContainText("you dropped a light");
});

test("switching streams never shows a region without a frame, nor a t going back, both ways", { tag: "@engines" }, async ({ page }) => {
  await openWall(page, 15, "direct");
  await page.getByTestId("play").click();
  await expect(page.getByTestId("streams-switch")).toHaveAttribute("aria-checked", "true");
  await page.evaluate(() => window.__fleet!.client.traceStart());
  await page.getByTestId("streams-switch").click();
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.getState().ui.shownMode), { timeout: 20_000 }).toBe("aggregated");
  await page.waitForTimeout(1500);
  await page.getByTestId("streams-switch").click();
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.getState().ui.shownMode), { timeout: 20_000 }).toBe("direct");
  await page.waitForTimeout(1500);
  const trace = await page.evaluate(() => window.__fleet!.client.traceStop());
  const modes = trace.map((e) => e.mode);
  expect(modes.indexOf("aggregated")).toBeGreaterThan(0);
  expect(modes.lastIndexOf("direct")).toBeGreaterThan(modes.indexOf("aggregated"));
  expect(trace.filter((e) => e.missing > 0)).toEqual([]);
  for (let i = 1; i < trace.length; i++) expect(trace[i]!.T!, `draw ${i}`).toBeGreaterThanOrEqual(trace[i - 1]!.T!);
  // The choice is remembered on this browser.
  expect(await page.evaluate(() => window.localStorage.getItem("fleet-wall.mode"))).toBe("direct");
});

test("a page that starts direct says it draws direct from its first plan", async ({ page }) => {
  await openWall(page, 15, "direct");
  // The worker's first plan names the mode it already draws, and the page still hears it.
  expect(await page.evaluate(() => window.__fleet!.client.getState().ui.shownMode)).toBe("direct");
  expect(await page.evaluate(() => Object.keys(window.__fleet!.client.getState().frames?.tiles ?? {}).every((k) => k.startsWith("d:")))).toBe(true);
  // So it does not call itself switching.
  await page.getByTestId("play").click();
  await expect(page.getByTestId("streams-note")).not.toContainText("switching");
});

test("aggregated is the default: no seam, and tile state drawn exactly as in direct mode", { tag: "@engines" }, async ({ page }) => {
  await page.goto("/?debug=1");
  await page.waitForFunction(() => window.__fleet !== undefined);
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.getState().ui.shownMode)).toBe("aggregated");
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.bench?.tilesSynced ?? 0), { timeout: 30_000 }).toBe(3);
  await page.waitForTimeout(1000);
  const r = await seamReport(page);
  console.log(`aggregated seams: T=${r.T} samples=${r.samples} worst tile ${Math.max(...Object.values(r.perTile)).toFixed(2)} worst boundary ${Math.max(...r.boundaries.map((b) => b.err)).toFixed(2)}`);
  for (const [id, sh] of Object.entries(r.shown)) if (sh !== "dark") expect(sh, id).toBe(`sync@${r.T}`);
  expect(Object.keys(r.shown).every((k) => k.startsWith("a:") || r.shown[k] === "dark")).toBe(true);
  for (const [id, e] of Object.entries(r.perTile)) expect(e, `tile ${id}`).toBeLessThan(TILE_ERR_MAX);
  for (const b of r.boundaries) expect(b.err, `${b.a} | ${b.b}`).toBeLessThan(ACROSS_ERR_MAX);
  await page.getByTestId("play").click();
  await expect(page.getByTestId("streams-switch")).toHaveAttribute("aria-checked", "false");
  await expect(page.getByTestId("streams-switch")).toHaveText("See every VM's own stream");
});

test("over the bandwidth budget, every direct tile asks for q=lo", async ({ page }) => {
  await page.addInitScript(() => window.localStorage.setItem("fleet-wall.mode", "direct"));
  // Under what 15 streams cost at std's floor (9 Mbit/s), whatever the screen.
  await page.goto("/?debug=1&budget=5");
  await page.waitForFunction(() => window.__fleet !== undefined && window.__fleet.view().plan.direct.length > 0);
  const urls = await page.evaluate(() => window.__fleet!.view().plan.direct.map((d) => d.url!));
  // All of them, so no lo tile sits beside a std one.
  expect(urls.filter((u) => u.includes("q=lo"))).toHaveLength(15);
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.bench?.tilesSynced ?? 0), { timeout: 30_000 }).toBeGreaterThanOrEqual(15);
});

test("a resize that moves the tiles to q=lo reopens their streams at q=lo, all of them together", async ({ page }) => {
  const stats = async () => (await (await fetch(`${COORD}/mock/render-stats`)).json()) as { subscribersByQuality: Record<string, number> };
  const plan = () => page.evaluate(() => window.__fleet!.view().plan);
  await page.addInitScript(() => window.localStorage.setItem("fleet-wall.mode", "direct"));
  // A budget between what the 15 streams cost at std on a 960x600 window and on a 1920x1080 one,
  // from the page's own layout and bit-rate rules.
  await page.setViewportSize({ width: 960, height: 600 });
  await page.goto("/?debug=1");
  await page.waitForFunction(() => window.__fleet !== undefined && window.__fleet.view().plan.direct.length > 0);
  const st = await page.evaluate(() => window.__fleet!.client.getState().accepted!);
  const cost = (w: number, h: number) =>
    layoutMosaic({ x: 0, y: 0, w, h }, st.tiles, 1, st.image_aspect).tiles.reduce((sum, p) => sum + streamBitrate(st.tiles.find((t) => t.id === p.id)!.rect, st.image_aspect, p.streamHeight, "std"), 0);
  const small = cost(960, 600);
  const big = cost(1920, 1080);
  expect(big).toBeGreaterThan(small * 1.5);
  const budget = (small + big) / 2;
  await page.goto(`/?debug=1&budget=${budget / 1e6}`);
  await page.waitForFunction(() => window.__fleet !== undefined && window.__fleet.view().plan.direct.length > 0);
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.bench?.tilesSynced ?? 0), { timeout: 30_000 }).toBeGreaterThanOrEqual(15);
  const before = await plan();
  expect(before.direct.some((d) => d.url!.includes("q=lo"))).toBe(false);
  await expect.poll(async () => (await stats()).subscribersByQuality.std ?? 0).toBeGreaterThanOrEqual(15);
  // A bigger screen asks for more pixels: the same budget now puts every tile at lo, on the same streams.
  await page.setViewportSize({ width: 1920, height: 1080 });
  await expect.poll(async () => (await plan()).direct.filter((d) => d.url!.includes("q=lo")).length, { timeout: 15_000 }).toBe(15);
  const after = await plan();
  console.log(`bps ${before.estimatedBps} -> ${after.estimatedBps}; budget ${Math.round(budget)}; every tile asked for q=lo`);
  // The renderer reads q only when a socket opens, so these tiles are served at lo only if their sockets were reopened.
  await expect.poll(async () => (await stats()).subscribersByQuality.lo ?? 0, { timeout: 15_000 }).toBe(15);
  // Every tile decodes frames from its reopened stream.
  const ids = after.direct.map((d) => d.id);
  const newest = () => page.evaluate((x) => x.map((id) => window.__fleet!.client.getState().frames?.tiles[id]?.newestT ?? null), ids);
  const reopened = await newest();
  await expect
    .poll(async () => (await newest()).filter((t, i) => t !== null && (reopened[i] === null || t > reopened[i]!)).length, { timeout: 15_000 })
    .toBe(15);
  // Back to the size this job's renderer is provisioned for (960x600, ci.yml): the tiles
  // reopen at std, and every one draws at the common t again. At 1920x1080 "all 15 in
  // sync" measures whether the mock keeps up; on one core it does not (6 of 15 after 33 s,
  // 3757 frames skipped against 2024 sent).
  await page.setViewportSize({ width: 960, height: 600 });
  await expect.poll(async () => (await stats()).subscribersByQuality.std ?? 0, { timeout: 15_000 }).toBe(15);
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.bench?.tilesSynced ?? 0), { timeout: 30_000 }).toBeGreaterThanOrEqual(15);
});

test("a coordinator with no poster yet shows no broken image", async ({ page }) => {
  // After a coordinator restart /poster.jpg answers 404 until a scene is captured.
  await admin("/mock/fault", { renderers_silent: true, no_poster: true });
  try {
    await page.goto("/?debug=1");
    await page.waitForFunction(() => window.__fleet !== undefined);
    await expect.poll(() =>
      page.evaluate(() => [...document.querySelectorAll("img")].filter((i) => i.complete && i.naturalWidth === 0 && i.getAttribute("src")).map((i) => `${i.className}|${i.getAttribute("src")}`)),
    ).toEqual([]);
    await expect(page.getByTestId("poster")).toHaveCount(0);
  } finally {
    await admin("/mock/fault", { renderers_silent: false, no_poster: false });
  }
});

test("first load paints the coordinator's poster at once, before any stream arrives", async ({ page }) => {
  // No renderer answers: the poster is all there is to show.
  await admin("/mock/fault", { renderers_silent: true });
  try {
    await page.goto("/?debug=1");
    await expect(page.getByTestId("poster")).toHaveAttribute("src", /\/poster\.jpg$/);
    await expect.poll(() => page.getByTestId("poster").evaluate((el) => (el as HTMLImageElement).naturalWidth)).toBe(1280);
    await page.waitForFunction(() => window.__fleet !== undefined);
    await expect.poll(() => page.evaluate(() => window.__fleet!.client.painted)).toBe(true);
    await page.evaluate(() => window.__fleet!.client.traceStart());
    const img = await page.evaluate(() => window.__fleet!.view().layout!.image);
    const res = await page.evaluate((p) => window.__fleet!.client.capture(p), [
      { x: img.x + img.w * 0.25, y: img.y + img.h * 0.5 },
      { x: img.x + img.w * 0.6, y: img.y + img.h * 0.3 },
    ]);
    const trace = await page.evaluate(() => window.__fleet!.client.traceStop());
    expect(trace.length).toBeGreaterThan(0);
    expect(trace.every((e) => e.poster)).toBe(true);
    // The poster's picture, not the dark of a region with no frame.
    for (const px of res.pixels) {
      expect(px.slice(0, 3)).not.toEqual([6, 8, 11]);
      expect(px[0] + px[1] + px[2]).toBeGreaterThan(30);
    }
    // The page's own copy (painted before the script ran) gives way to the canvas.
    await expect(page.getByTestId("poster")).toHaveClass(/gone/);
  } finally {
    await admin("/mock/fault", { renderers_silent: false });
  }
  // Streams arriving retire it: the regions fade in and the poster is no longer drawn.
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.bench?.tilesSynced ?? 0), { timeout: 30_000 }).toBe(3);
  await page.evaluate(() => window.__fleet!.client.traceStart());
  await page.waitForTimeout(800);
  const after = await page.evaluate(() => window.__fleet!.client.traceStop());
  expect(after.at(-1)!.poster).toBe(false);
});

test("pause holds the picture; resume moves it again", async ({ page }) => {
  await openWall(page, 15);
  await page.getByTestId("pause").click();
  await expect(page.getByTestId("pause")).toHaveAttribute("aria-pressed", "true");
  await expect(page.getByTestId("pause")).toHaveAttribute("aria-label", "Resume");
  const t0 = await drawnT(page);
  await page.waitForTimeout(1200);
  expect(await drawnT(page)).toBe(t0);
  await page.getByTestId("pause").click();
  await expect.poll(() => drawnT(page)).toBeGreaterThan(t0!);
});

test("a tile opened from a paused aggregated wall shows its own stream's frame and holds it", async ({ page }) => {
  await openWall(page, 3, "aggregated");
  await page.getByTestId("pause").click();
  await expect(page.getByTestId("pause")).toHaveAttribute("aria-pressed", "true");
  const t = await tileBox(page, "fcvm:3");
  await page.mouse.click(t.x + t.w / 2, t.y + t.h / 2);
  await expect(page.getByTestId("focus")).toBeVisible();
  // The opened tile is a direct stream the paused wall's frame choice never held.
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.getState().frames?.tiles["d:fcvm:3"]?.newestT ?? null), { timeout: 15_000 }).not.toBeNull();
  await expect.poll(async () => (await focusError(page, "fcvm:3")).shown, { timeout: 5_000 }).toMatch(/^(sync|behind)@/);
  const first = await focusError(page, "fcvm:3");
  // Still paused: the opened view holds that frame.
  await page.waitForTimeout(1200);
  const later = await focusError(page, "fcvm:3");
  expect(later.shown).toBe(first.shown);
  expect(later.err).toBeLessThan(TILE_ERR_MAX);
});

test("a paused picture survives frames arriving and a redraw: no tile goes dark", async ({ page }) => {
  await openWall(page, 15);
  await page.getByTestId("pause").click();
  await expect(page.getByTestId("pause")).toHaveAttribute("aria-pressed", "true");
  const t0 = await drawnT(page);
  // Streams keep sending while paused: far more than the ten frames a stream buffers.
  await page.waitForTimeout(3500);
  // A new size redraws the paused picture from the frozen choice.
  const vp = page.viewportSize()!;
  await page.setViewportSize({ width: vp.width - 160, height: vp.height - 100 });
  await expect.poll(() => page.evaluate(() => window.__fleet!.view().layout!.image.w)).toBeLessThan(vp.width - 100);
  // A 5x5 grid of points inside each live tile.
  const pts = await page.evaluate(() =>
    window.__fleet!.view().tiles.filter((t) => !t.dark && t.box).flatMap((t) =>
      Array.from({ length: 25 }, (_, k) => ({ id: t.id, x: t.box!.x + (t.box!.w * (1 + (k % 5))) / 6, y: t.box!.y + (t.box!.h * (1 + Math.floor(k / 5))) / 6 })),
    ),
  );
  expect(pts.length).toBeGreaterThan(0);
  const res = await page.evaluate((p) => window.__fleet!.client.capture(p), pts.map(({ x, y }) => ({ x, y })));
  expect(res.T).toBe(t0);
  // A tile with no frame is one flat fill (the dark fill, 6,8,11); the picture, even a night sky, varies across a tile.
  const flat = new Map<string, number>();
  pts.forEach((pt, i) => {
    const px = res.pixels[i]!;
    if (px[0] === 6 && px[1] === 8 && px[2] === 11) flat.set(pt.id, (flat.get(pt.id) ?? 0) + 1);
  });
  for (const id of new Set(pts.map((pt) => pt.id))) expect(flat.get(id) ?? 0, `tile ${id}: points drawn as the empty fill, of 25`).toBeLessThan(13);
});

test("a paused tile whose stream reopens gets a frame again, and the rest of the picture stays where it was", async ({ page }) => {
  await openWall(page, 15, "direct");
  await page.getByTestId("pause").click();
  await expect(page.getByTestId("pause")).toHaveAttribute("aria-pressed", "true");
  const T = await drawnT(page);
  // The VM fails and comes back (another viewer restarting it): its stream closes, dropping the frozen frame, and reopens.
  await admin("/mock/fault", { fail: "fcvm:2", message: "the VM did not come back", retry_ms: 2000 });
  await expect(page.getByTestId("dark-fcvm:2")).toHaveAttribute("data-kind", "failed");
  await expect(page.getByTestId("dark-fcvm:2")).toHaveCount(0, { timeout: 30_000 });
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.getState().frames?.tiles["d:fcvm:2"]?.newestT ?? null), { timeout: 30_000 }).not.toBeNull();
  await page.waitForTimeout(1500);
  const box = await tileBox(page, "fcvm:2");
  const pts = Array.from({ length: 25 }, (_, k) => ({ x: box.x + (box.w * (1 + (k % 5))) / 6, y: box.y + (box.h * (1 + Math.floor(k / 5))) / 6 }));
  const res = await page.evaluate((p) => window.__fleet!.client.capture(p), pts);
  // Still the picture paused at T, with a frame of its own in the reopened tile: not the flat empty fill (6,8,11).
  expect(res.T).toBe(T);
  const flat = res.pixels.filter((px) => px[0] === 6 && px[1] === 8 && px[2] === 11).length;
  expect(flat, "points of the reopened tile drawn as the empty fill, of 25").toBeLessThan(13);
  await expect(page.getByTestId("pause")).toHaveAttribute("aria-pressed", "true");
});

test("with less motion asked for, a tile going dark fades instead of flashing", async ({ page }) => {
  await openWall(page, 15, "direct");
  const darkOut = async (tile: string) => {
    await page.evaluate(() => window.__fleet!.client.traceStart());
    await page.evaluate((t) => window.__fleet!.client.send("relaunch", { tile: t }), tile);
    await expect(page.getByTestId(`dark-${tile}`)).toBeVisible();
    await page.waitForTimeout(900);
    const trace = await page.evaluate(() => window.__fleet!.client.traceStop());
    await expect(page.getByTestId(`dark-${tile}`)).toHaveCount(0, { timeout: 10_000 });
    return trace;
  };
  const sharp = await darkOut("fcvm:1");
  expect(sharp.filter((e) => e.darkIn > 0)).toEqual([]);
  await page.emulateMedia({ reducedMotion: "reduce" });
  const soft = await darkOut("fcvm:2");
  expect(soft.filter((e) => e.darkIn > 0).length).toBeGreaterThan(5);
});

test("failure states: a failed VM with its retry, a reclaimed host re-tiling, the coordinator offline with the picture still live", async ({ page }) => {
  await openWall(page, 15, "direct");
  await admin("/mock/fault", { fail: "fcvm:2", message: "GPU did not come back after reset", retry_ms: 4000 });
  const failed = page.getByTestId("dark-fcvm:2");
  await expect(failed).toHaveAttribute("data-kind", "failed");
  await expect(says(page, "fcvm VM 2")).toHaveAttribute("aria-label", /failed: GPU did not come back after reset, retrying in 0:0[1-4]/);
  // As the coordinator: the host's tiles leave the tiling at once and `failures` says why.
  await admin("/mock/fault", { reclaim: "mock-host-2" });
  await expect(page.getByTestId("banner-reclaimed")).toHaveText("AWS reclaimed an fcvm host. The image is re-tiled onto the GPUs left.");
  // The old tiling stays on screen until the new one has frames; then no place of that host is left.
  await expect(says(page, "fcvm VM 5")).toHaveCount(0, { timeout: 15_000 });
  await expect(page.getByTestId("dark-fcvm:5")).toHaveCount(0);
  await expect(page.getByTestId("restart-all")).toHaveText("Restart all 11 GPUs");
  await admin("/mock/fault", { replace_host: "mock-host-2" });
  await expect(page.getByTestId("banner-reclaimed")).toHaveCount(0);
  await expect(says(page, "fcvm VM 5")).toHaveCount(1, { timeout: 15_000 });
  await expect(page.getByTestId("dark-fcvm:5")).toHaveCount(0, { timeout: 15_000 });
  await expect(page.getByTestId("dark-fcvm:2")).toHaveCount(0, { timeout: 15_000 });
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.bench?.tilesSynced ?? 0), { timeout: 20_000 }).toBe(15);

  await admin("/mock/offline", { ms: 5000 });
  await expect(page.getByTestId("banner-offline")).toHaveText("The coordinator is offline. The picture still streams straight from the VMs; the controls are paused until it is back.");
  await expect(page.getByTestId("restart-all")).toBeDisabled();
  await expect(page.getByTestId("restart-note")).toHaveText("paused while the coordinator is offline");
  await expect(page.getByTestId("own-gpu")).toBeDisabled();
  // The streams come from the VMs, not the coordinator: the picture keeps moving.
  const t0 = await drawnT(page);
  await page.waitForTimeout(1000);
  expect(await drawnT(page)).toBeGreaterThan(t0! + 0.5);
  await expect(page.getByTestId("banner-offline")).toHaveCount(0, { timeout: 15_000 });
  await expect(page.getByTestId("restart-all")).toBeEnabled();
});

test("Turnstile: the first action waits for the check, then goes through", async ({ page, browser }) => {
  await admin("/mock/turnstile", { sitekey: "example-sitekey" });
  const stub = () => {
    window.turnstile = {
      render(el: HTMLElement, o: { callback: (t: string) => void }) {
        const b = document.createElement("button");
        b.textContent = "Verify (test)";
        b.onclick = () => o.callback("mock-pass");
        el.appendChild(b);
        return "w";
      },
      remove() {},
    };
  };
  try {
    await page.addInitScript(stub);
    await openWall(page, 15);
    await page.getByTestId("play").click();
    await expect(page.getByTestId("verify")).toBeVisible();
    await page.getByTestId("nudge-nebula").click();
    // Held, not sent: nothing reached the coordinator yet.
    await expect(page.getByTestId("nudge-nebula")).toContainText("0 votes");
    await page.getByRole("button", { name: "Verify (test)" }).click();
    await expect(page.getByTestId("verify")).toHaveCount(0);
    await expect(page.getByTestId("nudge-nebula")).toContainText("1 vote");
    await expect(page.getByTestId("nudge-nebula")).toHaveAttribute("aria-pressed", "true");

    // With the drawer closed, the first press brings the check to the button.
    const other = await (await browser.newContext()).newPage();
    await other.addInitScript(stub);
    await openWall(other, 15);
    await other.getByTestId("restart-all").click();
    await expect(other.locator(".verify-pop [data-testid=verify]")).toBeVisible();
    expect((await fleetNow()).relaunch_all?.by ?? "").not.toMatch(/^v-/);
    await other.context().close();
  } finally {
    await admin("/mock/turnstile", { sitekey: null });
  }
});

test("your own GPU: a wall tile's GPU is lent, streams full screen with its own camera for a minute, then snaps back into the wall", async ({ page, browser }) => {
  await openWall(page, 15);
  const other = await (await browser.newContext()).newPage();
  await openWall(other, 15);
  await page.getByTestId("own-gpu").click();
  await expect(page.getByTestId("own")).toBeVisible();
  await expect(page.getByTestId("own-timer")).toContainText("Click to first frame");
  // The other viewer sees the tile leave the image: a visitor's private GPU, with its seconds.
  const lent = other.getByTestId("dark-fcvm:8");
  await expect(lent).toHaveAttribute("data-kind", "borrowed");
  await expect(says(other, "fcvm VM 8")).toHaveAttribute("aria-label", /a visitor's private GPU, back in 0:\d\d/);
  // The cap: no second loan, and the button says why and when.
  await expect(other.getByTestId("own-gpu")).toBeDisabled();
  await expect(other.getByTestId("own-note")).toHaveText(/^the one GPU we lend is out; try again in 0:\d\d$/);
  await other.evaluate(() => window.__fleet!.client.act("claim_gpu"));
  await expect(other.getByTestId("own-refused")).toContainText(/^No GPU is free right now: all 1 GPUs we lend are out; the next comes back in \d+ s\. Try again in a minute\./);
  // The private stream arrives; the timer stops at the coordinator's click-to-first-frame.
  await expect(page.getByTestId("own-status")).toHaveText("Your GPU is streaming", { timeout: 10_000 });
  await expect(page.getByTestId("own-timer")).toContainText(/\d\.\d s$/);
  await expect(page.getByTestId("own-countdown")).toContainText(/Back to the pool in0:0\d/);
  await expect.poll(() => page.evaluate(() => Object.entries(window.__fleet!.client.getState().frames?.tiles ?? {}).some(([k, v]) => k.startsWith("o:") && v.newestT !== null)), { timeout: 15_000 }).toBe(true);
  // Its own camera: a drag moves it, and the page measures input to pixels.
  const shot = await page.getByTestId("own-shot").boundingBox();
  const dragStart = Date.now();
  await page.mouse.move(shot!.x + shot!.width / 2, shot!.y + shot!.height / 2);
  await page.mouse.down();
  for (let i = 1; i <= 8; i++) {
    await page.mouse.move(shot!.x + shot!.width / 2 + i * 12, shot!.y + shot!.height / 2);
    await page.waitForTimeout(25);
  }
  await page.mouse.up();
  // A real measurement: at least the 50 ms the mock puts a drag's camera key
  // ahead (no frame showing the drag exists sooner) and at most the time since
  // the drag began. No upper bound beyond that: on the mock it is the CPU the
  // software encoder and decoder get (about 300 ms on 4 cores, 1.1 s on 2).
  await expect(page.getByTestId("own-latency")).toHaveText(/^input to pixels: (\d+ ms|\d+(\.\d)? s)$/, { timeout: 10_000 });
  const latencyMs = await page.evaluate(() => window.__fleet!.client.getState().lastInputLatencyMs);
  expect(latencyMs).toBeGreaterThanOrEqual(50);
  expect(latencyMs).toBeLessThanOrEqual(Date.now() - dragStart);
  // The minute ends: back to the wall on its own, watching the borrowed tile restore.
  await expect(page.getByTestId("own-status")).toHaveText("Your minute is up. The GPU went back to the pool.", { timeout: 15_000 });
  await expect(page.getByTestId("own")).toHaveCount(0, { timeout: 5000 });
  await expect(page.getByTestId("hover-label")).toContainText(/your borrowed GPU(, coming back| is back)/);
  await expect(page.getByTestId("dark-fcvm:8")).toHaveCount(0, { timeout: 10_000 });
  const t = (await fleetNow()).tiles.find((x) => x.id === "fcvm:8")!;
  expect(t.borrowed).toBeNull();
  expect(t.phase).toBe("running");
  await expect(page.getByTestId("hover-label")).toHaveCount(0, { timeout: 10_000 });
  await other.context().close();
});

test("text is 14 px or larger at 4.5:1 or better, and race results go to a live region", async ({ page }) => {
  await openWall(page, 15);
  await page.getByTestId("play").click();
  const bad = await page.evaluate(() => {
    const parse = (c: string) => {
      const m = c.match(/rgba?\(([^)]+)\)/);
      if (!m) return [0, 0, 0, 0];
      const p = m[1]!.split(",").map((x) => Number(x.trim()));
      return [p[0]!, p[1]!, p[2]!, p[3] ?? 1];
    };
    const lum = ([r, g, b]: number[]) => {
      const f = (v: number) => {
        const s = v / 255;
        return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
      };
      return 0.2126 * f(r!) + 0.7152 * f(g!) + 0.0722 * f(b!);
    };
    // The colour behind an element: its own and its ancestors' backgrounds, over
    // the black ground (the picture under a backplate is taken as black too).
    const behind = (el: Element) => {
      const layers: number[][] = [];
      for (let e: Element | null = el; e; e = e.parentElement) {
        const c = parse(getComputedStyle(e).backgroundColor);
        if (c[3]! > 0) layers.push(c);
        if (c[3] === 1) break;
      }
      let out = [0, 0, 0];
      for (const c of layers.reverse()) out = out.map((v, i) => v * (1 - c[3]!) + c[i]! * c[3]!);
      return out;
    };
    const out: string[] = [];
    for (const el of document.querySelectorAll("body *")) {
      const own = [...el.childNodes].some((n) => n.nodeType === 3 && n.textContent!.trim() !== "");
      if (!own || el.closest(".sr-only") || el.closest("svg")) continue;
      const r = el.getBoundingClientRect();
      const cs = getComputedStyle(el);
      if (r.width === 0 || r.height === 0 || cs.visibility === "hidden" || cs.display === "none") continue;
      const size = parseFloat(cs.fontSize);
      const fg = parse(cs.color);
      const bg = behind(el);
      const a = lum(fg) + 0.05;
      const b = lum(bg) + 0.05;
      const ratio = Math.max(a, b) / Math.min(a, b);
      if (size < 14 || ratio < 4.5) out.push(`${el.tagName}.${el.className} "${el.textContent!.trim().slice(0, 30)}" ${size}px ${ratio.toFixed(2)}:1`);
    }
    return out;
  });
  expect(bad).toEqual([]);
  await expect(page.getByTestId("announce")).toHaveAttribute("aria-live", "polite");
  await expect(page.getByTestId("announce")).toContainText("EC2: all 7 GPUs streaming again");
});

/** Make document.visibilityState settable from the test, and switch it. */
const visibility = {
  install: (page: Page) =>
    page.addInitScript(() => {
      let state: DocumentVisibilityState = "visible";
      Object.defineProperty(document, "visibilityState", { get: () => state, configurable: true });
      Object.defineProperty(document, "hidden", { get: () => state === "hidden", configurable: true });
      (window as unknown as { __setVisibility: (v: DocumentVisibilityState) => void }).__setVisibility = (v) => {
        state = v;
        document.dispatchEvent(new Event("visibilitychange"));
      };
    }),
  set: (page: Page, v: "hidden" | "visible") => page.evaluate((x) => (window as unknown as { __setVisibility: (v: string) => void }).__setVisibility(x), v),
};

const subscribers = async () => ((await (await fetch(`${COORD}/mock/render-stats`)).json()) as { subscribers: number }).subscribers;

test("a tab hidden past the limit closes every frame stream and keeps the coordinator socket; shorter, nothing changes", async ({ page }) => {
  await visibility.install(page);
  // The limit is HIDDEN_PAUSE_MS (5 minutes); the debug page takes a shorter one.
  await page.addInitScript(() => window.localStorage.setItem("fleet-wall.mode", "direct"));
  await page.goto("/?debug=1&hidden_ms=1500");
  await page.waitForFunction(() => window.__fleet !== undefined);
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.bench?.tilesSynced ?? 0), { timeout: 30_000 }).toBe(15);
  const open = await subscribers();
  expect(open).toBeGreaterThanOrEqual(15);
  // Hidden for less than the limit: the streams stay.
  await visibility.set(page, "hidden");
  await page.waitForTimeout(700);
  await visibility.set(page, "visible");
  await page.waitForTimeout(1200);
  expect(await subscribers()).toBe(open);
  expect(await page.evaluate(() => window.__fleet!.client.getState().ui.suspended)).toBe(false);
  // Hidden past it: every frame stream closes, the coordinator socket stays and state keeps coming.
  await visibility.set(page, "hidden");
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.getState().ui.suspended), { timeout: 5000 }).toBe(true);
  await expect.poll(subscribers, { timeout: 5000 }).toBe(0);
  await expect.poll(() => page.evaluate(() => Object.keys(window.__fleet!.client.getState().frames?.tiles ?? {}).length)).toBe(0);
  const v0 = await page.evaluate(() => window.__fleet!.client.getState().version);
  await setTimeOfDay(0.5);
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.getState().version)).toBeGreaterThan(v0);
  expect(await page.evaluate(() => window.__fleet!.client.getState().connection)).toBe("open");
  await visibility.set(page, "visible");
  await expect.poll(subscribers, { timeout: 10_000 }).toBeGreaterThanOrEqual(15);
  await setTimeOfDay(0.42);
});

test("coming back to the tab holds the last picture, then fades the live image in at one common t", async ({ page }) => {
  await visibility.install(page);
  await page.addInitScript(() => window.localStorage.setItem("fleet-wall.mode", "direct"));
  await page.goto("/?debug=1&hidden_ms=800");
  await page.waitForFunction(() => window.__fleet !== undefined);
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.bench?.tilesSynced ?? 0), { timeout: 30_000 }).toBe(15);
  const img = await page.evaluate(() => window.__fleet!.view().layout!.image);
  const point = [{ x: img.x + img.w * 0.3, y: img.y + img.h * 0.4 }];
  await visibility.set(page, "hidden");
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.getState().ui.suspended)).toBe(true);
  const tHidden = await page.evaluate(() => window.__fleet!.client.getState().frames!.T!);
  await expect.poll(subscribers, { timeout: 5000 }).toBe(0);
  const before = (await page.evaluate((p) => window.__fleet!.client.capture(p), point)).pixels[0]!;
  await page.evaluate(() => window.__fleet!.client.traceStart());
  await visibility.set(page, "visible");
  // At once: the picture from before, not dark.
  const at = await page.evaluate((p) => window.__fleet!.client.capture(p), point);
  expect(at.pixels[0]!.slice(0, 3)).toEqual(before.slice(0, 3));
  expect(at.pixels[0]!.slice(0, 3)).not.toEqual([6, 8, 11]);
  // The drawn t moves on only once the live image is back (the held picture keeps the old one).
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.getState().frames?.T ?? 0), { timeout: 15_000 }).toBeGreaterThan(tHidden);
  await page.waitForTimeout(800);
  const trace = await page.evaluate(() => window.__fleet!.client.traceStop());
  const firstLive = trace.findIndex((e) => !e.held || e.T !== trace[0]!.T);
  expect(trace[0]!.held).toBe(true);
  expect(firstLive).toBeGreaterThan(0);
  // No region ever drawn without a frame, the held picture under the fade, and t never going back.
  expect(trace.filter((e) => e.missing > 0)).toEqual([]);
  expect(trace.some((e) => e.held && e.T !== trace[0]!.T)).toBe(true);
  for (let i = 1; i < trace.length; i++) expect(trace[i]!.T!, `draw ${i}`).toBeGreaterThanOrEqual(trace[i - 1]!.T!);
  expect(trace.at(-1)!.held).toBe(false);
});

test("a paused wall hidden past the limit comes back with a picture, and holds it", async ({ page }) => {
  await visibility.install(page);
  await page.addInitScript(() => window.localStorage.setItem("fleet-wall.mode", "direct"));
  await page.goto("/?debug=1&hidden_ms=800");
  await page.waitForFunction(() => window.__fleet !== undefined);
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.bench?.tilesSynced ?? 0), { timeout: 30_000 }).toBe(15);
  await page.getByTestId("pause").click();
  await expect(page.getByTestId("pause")).toHaveAttribute("aria-pressed", "true");
  await visibility.set(page, "hidden");
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.getState().ui.suspended)).toBe(true);
  // Suspension closed every stream and dropped its frames; the frozen choice still names them.
  await expect.poll(subscribers, { timeout: 5000 }).toBe(0);
  await page.evaluate(() => window.__fleet!.client.traceStart());
  await visibility.set(page, "visible");
  await expect.poll(subscribers, { timeout: 15_000 }).toBe(15);
  // Past the hold and its fade.
  await page.waitForTimeout(4000);
  const trace = await page.evaluate(() => window.__fleet!.client.traceStop());
  // No region drawn without a frame: the frozen choice named frames suspension had dropped.
  expect(trace.filter((e) => e.missing > 0)).toEqual([]);
  // The picture is the reopened streams' frames, held while still paused: no tile is the flat empty fill (6,8,11).
  const T = await page.evaluate(() => window.__fleet!.client.getState().frames!.T);
  await page.waitForTimeout(1200);
  const pts = await page.evaluate(() =>
    window.__fleet!.view().tiles.filter((t) => !t.dark && t.box).flatMap((t) =>
      Array.from({ length: 25 }, (_, k) => ({ id: t.id, x: t.box!.x + (t.box!.w * (1 + (k % 5))) / 6, y: t.box!.y + (t.box!.h * (1 + Math.floor(k / 5))) / 6 })),
    ),
  );
  const res = await page.evaluate((p) => window.__fleet!.client.capture(p), pts.map(({ x, y }) => ({ x, y })));
  expect(res.T).toBe(T);
  const flat = new Map<string, number>();
  pts.forEach((pt, i) => {
    const px = res.pixels[i]!;
    if (px[0] === 6 && px[1] === 8 && px[2] === 11) flat.set(pt.id, (flat.get(pt.id) ?? 0) + 1);
  });
  for (const id of new Set(pts.map((pt) => pt.id))) expect(flat.get(id) ?? 0, `tile ${id}: points drawn as the empty fill, of 25`).toBeLessThan(13);
  await expect(page.getByTestId("pause")).toHaveAttribute("aria-pressed", "true");
  // The held picture was let go once the live one had faded in, though the frozen picture stopped drawing.
  expect(trace.at(-1)!.held).toBe(false);
});

const reelDate = async () => utcDate(((await (await fetch(`${COORD}/media/wall-reel.json`)).json()) as { recorded_at: number }).recorded_at);
const power = async () => ((await mockState()).state as unknown as { fleet: { power: { state: string } } }).fleet.power.state;
const sleepNow = () => admin("/mock/power", { sleep: true, hold: true });

/** Wake the fleet and wait until the live image has replaced the recording. */
async function wakeUp(page: Page): Promise<void> {
  await admin("/mock/power", { wake: true });
  await expect.poll(power, { timeout: 20_000 }).toBe("awake");
  await expect(page.getByTestId("reel")).toHaveCount(0, { timeout: 20_000 });
}

test("asleep: the recording plays over the poster with its badge, the scores are dated, the buttons wait; waking shows the real phases, then the live image takes over with no blank frame", async ({ page }) => {
  await openWall(page, 15);
  // The scores are dated while asleep: make sure one race has been measured.
  if ((await fleetNow()).scoreboard.fcvm === null) {
    await admin("/mock/schedule", { clear_cooldown: true });
    await page.evaluate(() => window.__fleet!.client.send("restart_now", {}));
    await expect.poll(async () => (await fleetNow()).scoreboard.fcvm !== null, { timeout: 40_000 }).toBe(true);
    await raceOver();
  }
  await sleepNow();
  await expect(page.getByTestId("sleep-card")).toContainText("Asleep.");
  await expect(page.locator(".dark-tile")).toHaveCount(0);
  await expect(page.getByTestId("reel")).toHaveAttribute("data-state", "playing", { timeout: 15_000 });
  const t0 = await page.getByTestId("reel-video").evaluate((v) => (v as HTMLVideoElement).currentTime);
  await page.waitForTimeout(600);
  expect(await page.getByTestId("reel-video").evaluate((v) => (v as HTMLVideoElement).currentTime)).not.toBe(t0);
  expect(await page.getByTestId("reel-video").evaluate((v) => { const x = v as HTMLVideoElement; return [x.muted, x.loop, x.playsInline, x.textTracks[0]?.mode]; })).toEqual([true, true, true, "showing"]);
  await expect(page.getByTestId("reel-badge")).toContainText(`◉ RECORDING · ${await reelDate()}`);
  await expect(page.getByTestId("last-race")).toHaveText(/^Last race: fcvm back in .+ · measured \d+ \w+ \d{4}, \d\d:\d\d UTC$/);
  await expect(page.getByTestId("restart-note")).toHaveText("works once the GPUs are awake");
  await expect(page.getByTestId("own-note")).toHaveText("works once the GPUs are awake");
  await expect(page.getByTestId("restart-all")).toBeDisabled();
  await expect(page.getByTestId("restart-all")).toHaveText("Restart all GPUs");

  // Waking: a live clock since the wake began and each phase as the hosts report it.
  await page.evaluate(() => window.__fleet!.client.traceStart());
  // Every change of the recording's leaving state, sampled each animation frame until it goes.
  await page.evaluate(() => {
    const w = window as unknown as { __leaving: boolean[] };
    w.__leaving = [];
    const tick = () => {
      const el = document.querySelector(".reel");
      if (!el) return;
      const on = el.classList.contains("leaving");
      if (w.__leaving.at(-1) !== on) w.__leaving.push(on);
      requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
  });
  await admin("/mock/power", { wake: true });
  await expect(page.getByTestId("sleep-card")).toContainText(/WAKING (\d+ GPUs|THE GPUs)/);
  await expect(page.getByTestId("reel-badge")).toContainText(`◉ RECORDING · ${await reelDate()}`);
  await expect(page.getByTestId("wake-clock")).toHaveText(/^0:0\d\.\d$/);
  await expect(page.getByTestId("wake-instance_pending")).toHaveAttribute("data-state", "done");
  await expect(page.getByTestId("wake-booted")).toHaveAttribute("data-state", "done", { timeout: 10_000 });
  // The live image takes over only once it is whole: at the moment the recording starts to fade, every region has a frame.
  await page.waitForSelector(".reel.leaving", { timeout: 25_000 });
  expect(await page.evaluate(() => window.__fleet!.client.getState().frames!.allLive)).toBe(true);
  const img = await page.evaluate(() => window.__fleet!.view().layout!.image);
  const pts = [0.1, 0.3, 0.45, 0.6, 0.8].flatMap((u) => [0.2, 0.7].map((v) => ({ x: img.x + u * img.w, y: img.y + v * img.h })));
  const shot = await page.evaluate((p) => window.__fleet!.client.capture(p), pts);
  for (const px of shot.pixels) expect(px.slice(0, 3)).not.toEqual([6, 8, 11]);
  await expect(page.getByTestId("reel")).toHaveCount(0, { timeout: 5000 });
  // Once the recording starts to leave, the live image under it stays whole: it never comes back.
  expect(await page.evaluate(() => (window as unknown as { __leaving: boolean[] }).__leaving)).toEqual([false, true]);
  const trace = await page.evaluate(() => window.__fleet!.client.traceStop());
  expect(trace.filter((e) => e.T !== null && e.missing > 0 && !e.poster)).toEqual([]);
  // The wake, measured once, in the drawer's About section.
  await page.getByTestId("menu").click();
  await expect(page.getByTestId("wake-stat")).toHaveText(/^Last wake: 15 GPUs from zero in 0:\d\d \(\d+ \w+ \d{4}, \d\d:\d\d UTC\)$/);
  await expect(page.getByTestId("restart-all")).toBeEnabled();
});

test("with less motion asked for, the poster stays and a button plays the recording", async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  await openWall(page, 15);
  await sleepNow();
  try {
    await expect(page.getByTestId("reel")).toHaveAttribute("data-state", "manual");
    await expect(page.getByTestId("reel-play")).toHaveText("Play the recording");
    await expect(page.getByTestId("reel")).toContainText("Not started by itself, as your device asks for less motion.");
    await page.waitForTimeout(800);
    expect(await page.getByTestId("reel-video").evaluate((v) => (v as HTMLVideoElement).paused)).toBe(true);
    await page.getByTestId("reel-play").click();
    await expect(page.getByTestId("reel")).toHaveAttribute("data-state", "playing", { timeout: 15_000 });
  } finally {
    await wakeUp(page);
  }
});

test("a missing recording leaves the poster, with nothing broken on top", async ({ page }) => {
  await admin("/mock/media", { missing: true });
  try {
    await openWall(page, 15);
    await page.evaluate(() => window.__fleet!.client.traceStart());
    await sleepNow();
    await expect(page.getByTestId("reel")).toHaveAttribute("data-state", "failed", { timeout: 15_000 });
    await expect(page.getByTestId("reel-video")).toHaveCount(0);
    await expect(page.getByTestId("reel-play")).toHaveCount(0);
    await expect(page.getByTestId("reel-badge")).toHaveCount(0);
    await page.waitForTimeout(500);
    const trace = await page.evaluate(() => window.__fleet!.client.traceStop());
    expect(trace.at(-1)!.poster).toBe(true);
    await expect(page.getByTestId("sleep-card")).toContainText("Asleep.");
  } finally {
    await admin("/mock/media", { missing: false });
    await wakeUp(page);
  }
});

test("hidden tabs do not count as viewers", async ({ browser }) => {
  const a = await browser.newContext();
  const b = await browser.newContext();
  const pa = await a.newPage();
  const pb = await b.newPage();
  await visibility.install(pb);
  await openWall(pa, 15);
  await openWall(pb, 15);
  const watching = () => pa.getByTestId("viewers").textContent();
  const n = Number((await watching())!.match(/^(\d+)/)![1]);
  await visibility.set(pb, "hidden");
  await expect(pa.getByTestId("viewers")).toHaveText(new RegExp(`^${n - 1} (person|people) watching$`));
  await visibility.set(pb, "visible");
  await expect(pa.getByTestId("viewers")).toHaveText(new RegExp(`^${n} (person|people) watching$`));
  await a.close();
  await b.close();
});

test("touch: a long press shows the pressed tile's seams and label", async ({ browser }) => {
  // A touch screen at one device pixel per CSS pixel: the mock renders every
  // pixel in JavaScript, so a 2.6x screen would ask it for more than it can draw.
  const ctx = await browser.newContext({ hasTouch: true, isMobile: true, viewport: { width: 1000, height: 700 }, deviceScaleFactor: 1 });
  const page = await ctx.newPage();
  await openWall(page, 15);
  const t = await tileBox(page, "fcvm:5");
  const cdp = await ctx.newCDPSession(page);
  const x = t.x + t.w / 2;
  const y = t.y + t.h / 2;
  await cdp.send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: [{ x, y }] });
  await page.waitForTimeout(700);
  await expect(page.getByTestId("seams")).toHaveAttribute("data-visible", "1");
  await expect(page.getByTestId("hover-outline")).toHaveAttribute("data-tile", "fcvm VM 5");
  await cdp.send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
  await expect(page.getByTestId("seams")).toHaveAttribute("data-visible", "0");
  await expect(page.getByTestId("focus")).toHaveCount(0);
  await ctx.close();
});

test("phone: the image full width at the top, the strip under it, one button in a dock, and a hint to turn the phone", async ({ browser }) => {
  const ctx = await browser.newContext({ ...devices["iPhone 13"], deviceScaleFactor: 1 });
  const page = await ctx.newPage();
  await page.goto("/?debug=1");
  await page.waitForFunction(() => window.__fleet !== undefined && window.__fleet.view().layout !== null);
  const vw = page.viewportSize()!.width;
  const vh = page.viewportSize()!.height;
  const img = await page.evaluate(() => window.__fleet!.view().layout!.image);
  expect(img.w).toBe(vw);
  expect(img.y).toBeLessThan(80);
  // The tags sit on the image.
  const tag = await page.getByTestId("side-fcvm").boundingBox();
  expect(tag!.y).toBeGreaterThanOrEqual(img.y);
  expect(tag!.y + tag!.height).toBeLessThanOrEqual(img.y + img.h);
  await expect(page.getByTestId("spec")).toHaveText(/^\d+×\d+ · \d+ × .+ · path traced$|^\d+×\d+ · \d+ × .+ · \d+ fps · path traced$/);
  // One button, in a dock in the thumb zone; the drawer is a text link and a menu icon.
  await expect(page.locator(".phone .btn:visible")).toHaveCount(1);
  const dock = await page.getByTestId("restart-all").boundingBox();
  expect(dock!.y + dock!.height).toBeLessThanOrEqual(vh);
  expect(dock!.y).toBeGreaterThan(vh / 2);
  await page.getByTestId("phone-play").click();
  await expect(page.getByTestId("drawer")).toBeVisible();
  await page.getByTestId("text-style-landmark").click();
  await expect(page.getByRole("radio", { name: "Landmark" })).toBeChecked();
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);
  await page.getByRole("button", { name: "Close" }).click();
  await expect(page.getByTestId("restart-all")).toBeVisible();
  await expect(page.getByTestId("rotate-hint")).toHaveText("Turn sideways for full screen");
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
  expect(overflow).toBeLessThanOrEqual(0);
  await ctx.close();
});

test("phone, landscape: full-bleed like the desktop", async ({ browser }) => {
  const ctx = await browser.newContext({ ...devices["iPhone 13 landscape"], deviceScaleFactor: 1 });
  const page = await ctx.newPage();
  await page.goto("/?debug=1");
  await page.waitForFunction(() => window.__fleet !== undefined && window.__fleet.view().layout !== null);
  const v = page.viewportSize()!;
  const img = await page.evaluate(() => window.__fleet!.view().layout!.image);
  // Contain-fit to the whole screen: the full height, centred.
  expect(img.h).toBe(v.height);
  expect(Math.abs(img.x + img.w / 2 - v.width / 2)).toBeLessThanOrEqual(1);
  await expect(page.locator(".hud-top")).toBeVisible();
  await expect(page.getByTestId("restart-all")).toBeVisible();
  await ctx.close();
});

test("one address family for both halves: a viewer with no IPv6 gets IPv4 for every tile; with IPv6, IPv6 for every tile", async ({ page }) => {
  const byFamily = async () => ((await (await fetch(`${COORD}/mock/render-stats`)).json()) as { subscribersByFamily: Record<string, number> }).subscribersByFamily;
  const net = () => page.evaluate(() => window.__fleet!.client.getState().net);
  const hosts = () =>
    page.evaluate(() => {
      const byHalf: Record<string, string[]> = {};
      for (const d of window.__fleet!.view().plan.direct) (byHalf[d.id.split(":")[1]!] ??= []).push(new URL(d.url!).hostname);
      return byHalf;
    });
  await page.addInitScript(() => window.localStorage.setItem("fleet-wall.mode", "direct"));

  await admin("/mock/fault", { ipv6_down: true });
  try {
    await page.goto("/?debug=1");
    await page.waitForFunction(() => window.__fleet?.client.getState().net.family != null, undefined, { timeout: 10_000 });
    expect((await net()).family).toBe("v4");
    const v4 = await hosts();
    expect(Object.keys(v4).sort()).toEqual(["ec2", "fcvm"]);
    for (const [half, h] of Object.entries(v4)) expect(new Set(h), half).toEqual(new Set(["127.0.0.1"]));
    await page.evaluate(() => window.__fleet!.client.ui({ drawer: true }));
    await expect(page.getByTestId("family")).toHaveText(/^Address family: IPv4 for every tile \(connected first, in \d+ ms\)$/);
    const n = v4["fcvm"]!.length + v4["ec2"]!.length;
    await expect.poll(async () => (await byFamily()).v4 ?? 0, { timeout: 15_000 }).toBe(n);
    expect((await byFamily()).v6 ?? 0).toBe(0);
    await expect.poll(() => page.evaluate(() => window.__fleet!.client.bench?.tilesSynced ?? 0), { timeout: 30_000 }).toBeGreaterThanOrEqual(n);
  } finally {
    await admin("/mock/fault", { ipv6_down: false });
  }

  // A new page load probes again: IPv6 works now, so every tile goes over it.
  await page.reload();
  await page.waitForFunction(() => window.__fleet?.client.getState().net.family != null, undefined, { timeout: 10_000 });
  const n6 = await net();
  expect(n6.family).toBe("v6");
  expect(n6.probeMs).toBeLessThan(1500);
  const v6 = await hosts();
  expect(Object.keys(v6).sort()).toEqual(["ec2", "fcvm"]);
  for (const [half, h] of Object.entries(v6)) expect(new Set(h), half).toEqual(new Set(["localhost"]));
  await page.evaluate(() => window.__fleet!.client.ui({ drawer: true }));
  await expect(page.getByTestId("family")).toHaveText(/^Address family: IPv6 for every tile \(connected first, in \d+ ms\)$/);
  const m = v6["fcvm"]!.length + v6["ec2"]!.length;
  await expect.poll(async () => (await byFamily()).v6 ?? 0, { timeout: 15_000 }).toBe(m);
  await expect.poll(async () => (await byFamily()).v4 ?? 0, { timeout: 15_000 }).toBe(0);
});

test("a probe that lands while the renderers are not accepting yet tries again: the page gets IPv6, not IPv4 with every tile", async ({ page }) => {
  await page.addInitScript(() => window.localStorage.setItem("fleet-wall.mode", "direct"));
  // As at the end of a wake: the tiles say running, and their renderers refuse every socket for a moment.
  await admin("/mock/fault", { renderers_silent: true });
  try {
    await page.goto("/?debug=1");
    await page.waitForFunction(() => window.__fleet?.client.getState().accepted != null);
    await page.waitForTimeout(600);
  } finally {
    await admin("/mock/fault", { renderers_silent: false });
  }
  await page.waitForFunction(() => window.__fleet?.client.getState().net.family != null, undefined, { timeout: 10_000 });
  expect((await page.evaluate(() => window.__fleet!.client.getState().net)).family).toBe("v6");
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.bench?.tilesSynced ?? 0), { timeout: 30_000 }).toBeGreaterThanOrEqual(15);
});
