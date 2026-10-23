import { expect, test, devices, type Page } from "@playwright/test";
import { seconds, thousands, utcDate } from "../core/format.ts";
import { COORD, admin, fleetNow, focusError, mockState, openWall, raceOver, seamReport, setSkew } from "./helpers.ts";

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
    await page.waitForTimeout(1500);
    const r = await seamReport(page);
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

test("restart all: the race is the hero, one counter per half, fcvm's bar stops while EC2's grows, results announced", async ({ page, browser }) => {
  await openWall(page, 15);
  const watcher = await (await browser.newContext()).newPage();
  await openWall(watcher, 15);
  await expect(page.getByTestId("restart-all")).toHaveText("Restart all 15 GPUs");
  await expect(page.getByTestId("announce")).toHaveAttribute("aria-live", "polite");
  await page.getByTestId("restart-all").click();
  await expect(page.getByTestId("restart-all")).toBeDisabled();
  await expect(page.getByTestId("restart-all")).toHaveText("Restarting now");
  await expect(page.getByTestId("countdown")).toContainText("Restart running");
  await expect(watcher.getByTestId("countdown")).toContainText("Restart running");
  // No per-tile clock: a dark tile shows how far its restart is.
  await expect(page.locator('[data-testid^="dark-fcvm"] svg.glyph.progress').first()).toBeVisible();
  await expect(page.locator(".dark-tile .clock")).toHaveCount(0);
  // fcvm comes back: its counter freezes, with a check mark, while EC2's runs on.
  await expect(page.getByTestId("bar-fcvm")).toHaveClass(/done/, { timeout: 10_000 });
  await expect(page.getByTestId("line-fcvm")).toHaveText("all 8 GPUs streaming again");
  const a = { f: await page.getByTestId("value-fcvm").textContent(), e: await page.getByTestId("value-ec2").textContent(), fs: Number(await page.getByTestId("bar-fcvm").getAttribute("data-solid")), es: Number(await page.getByTestId("bar-ec2").getAttribute("data-solid")) };
  await page.waitForTimeout(1200);
  const b = { f: await page.getByTestId("value-fcvm").textContent(), e: await page.getByTestId("value-ec2").textContent(), fs: Number(await page.getByTestId("bar-fcvm").getAttribute("data-solid")), es: Number(await page.getByTestId("bar-ec2").getAttribute("data-solid")) };
  expect(a.f).toMatch(/^\d\.\d s$/);
  expect(b.f).toBe(a.f);
  expect(b.fs).toBeLessThanOrEqual(a.fs);
  expect(b.e).not.toBe(a.e);
  expect(b.es).toBeGreaterThan(a.es);
  await expect(page.getByTestId("announce")).toHaveText(/^fcvm: all 8 GPUs streaming again \d\.\d s after the restart\.$/);
  // EC2 comes back; both results are announced and both bars show what they were measured on.
  await expect(page.getByTestId("announce")).toContainText(/EC2: all 7 GPUs streaming again \d+ s after the restart\./, { timeout: 30_000 });
  await expect(page.getByTestId("measured-fcvm")).toHaveText("measured on 8 GPUs");
  await expect(page.getByTestId("measured-ec2")).toHaveText("measured on 7 GPUs");
  const st = await fleetNow();
  const ra = st.relaunch_all!;
  expect(ra.fcvm_done_at! - ra.started_at).toBeLessThan(ra.ec2_done_at! - ra.started_at);
  // Every number is the coordinator's: the last restart, as it measured it.
  await expect(page.getByTestId("line-fcvm")).toHaveText(`last restart ${seconds(st.scoreboard.fcvm!.last_ms)}`);
  await expect(page.getByTestId("value-ec2")).toHaveText(seconds(st.scoreboard.ec2!.median_ms));
  await expect(page.getByTestId("restart-all")).toBeEnabled();
  await expect(page.getByTestId("feed")).toContainText("you restarted every GPU");
  await expect(watcher.getByTestId("feed")).toContainText(/viewer \d+ restarted every GPU/);
  await watcher.context().close();
});

test("the owner's schedule: a shared countdown, a restart by the schedule, a cooldown for everyone, and a capacity wait drawn hatched and not counted", async ({ page, browser }) => {
  await openWall(page, 15);
  const other = await (await browser.newContext()).newPage();
  await openWall(other, 15);
  await admin("/mock/schedule", { next_in_ms: 4000, cooldown_s: 40, capacity_wait_ms: 3000 });
  await expect(page.getByTestId("countdown")).toContainText("Next restart in");
  await expect(page.getByTestId("countdown-value")).toHaveText(/^0:0[1-4]$/);
  await expect(other.getByTestId("countdown-value")).toHaveText(/^0:0[1-4]$/);
  await expect(page.getByTestId("countdown")).toContainText("Scheduled restart, running", { timeout: 8000 });
  await expect(page.getByTestId("feed")).toContainText("the scheduled restart of every GPU began");
  // While AWS has no spot GPU: hatched, first on EC2's bar, labelled, and the tiles say so.
  await expect(page.getByTestId("hatch-ec2")).toBeVisible();
  await expect(page.getByTestId("hatch-label-ec2")).toHaveText(/^AWS had no spot GPU: waiting 0:0\d, not counted$/);
  await expect(page.getByTestId("dark-ec2:1")).toHaveAttribute("data-kind", "capacity");
  await expect(page.locator(".key-grid button[aria-label^='EC2 VM 1,']")).toHaveAttribute("aria-label", /AWS had no spot GPU/);
  const track = page.getByTestId("bar-ec2").locator(".track > span").first();
  await expect(track).toHaveClass("hatch");
  await raceOver();
  const st = await fleetNow();
  expect(st.relaunch_all!.by).toBe("schedule");
  expect(st.scoreboard.ec2!.last_capacity_wait_ms).toBe(3000);
  // The headline leaves the wait out: EC2's last time is click to last frame less the wait.
  expect(st.scoreboard.ec2!.last_ms).toBe(st.relaunch_all!.ec2_done_at! - st.relaunch_all!.started_at - 3000);
  await expect(page.getByTestId("line-ec2")).toHaveText(`last restart ${seconds(st.scoreboard.ec2!.last_ms)}`);
  await expect(page.getByTestId("hatch-label-ec2")).toHaveText("last restart: AWS had no spot GPU for 3.0 s, not counted");
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
  const gen0 = await page.evaluate(() => window.__fleet!.client.getState().accepted!.layout.gen);
  await page.evaluate(() => window.__fleet!.client.traceStart());
  await admin("/mock/fault", { fcvm_gpus: fcvm, ec2_gpus: ec2 });
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.getState().shownLayout?.gen), { timeout: 15_000 }).toBe(gen0 + 1);
  const atT = await page.evaluate(() => window.__fleet!.client.getState().accepted!.layout.at_frame / 30);
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
      if (late !== null && text.includes('"type":"snapshot"') && (JSON.parse(text) as { state: { layout: { gen: number } } }).state.layout.gen >= late) {
        setTimeout(() => ws.send(text), 1500);
        return;
      }
      ws.send(m);
    });
    ws.onMessage((m) => server.send(m));
  });
  await openWall(page, 15);
  late = (await page.evaluate(() => window.__fleet!.client.getState().accepted!.layout.gen)) + 1;
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
    await expect(page.getByTestId("caption-ec2")).toHaveCount(0);
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
  await expect(label).toContainText("g4dn.xlarge");
  await expect(label).toContainText(/no restart yet|last restart \d/);
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

test("an opened VM streams at its native size, with its restart timeline and live GPU readings", async ({ page }) => {
  await openWall(page, 15);
  const t = await tileBox(page, "fcvm:3");
  await page.mouse.click(t.x + t.w / 2, t.y + t.h / 2);
  await expect(page.getByTestId("focus")).toBeVisible();
  const placed = await page.evaluate(() => window.__fleet!.view().focus!.placed!);
  const wanted = await page.evaluate(() => window.__fleet!.view().plan.direct.find((x) => x.id === "d:fcvm:3")!.height);
  expect(wanted).toBe(placed.streamHeight);
  expect(wanted).toBeLessThanOrEqual(720);
  // 720p at one device pixel per pixel when it fits; on a smaller screen the box
  // shrinks and asks for the next stream height up from what it covers.
  expect(wanted).toBeGreaterThanOrEqual(placed.box.h);
  if (placed.box.h < 720) expect(await page.evaluate(() => window.__fleet!.view().focus!.placed!.box.h < 720)).toBe(true);
  else expect(placed.box.h).toBe(720);
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.getState().frames!.tiles["d:fcvm:3"]!.height), { timeout: 15_000 }).toBe(wanted);
  await expect(page.getByTestId("focus-stream")).toHaveText(new RegExp(`^${wanted}p · \\d+ fps · (native size|shown at \\d+%)$`));
  const others = await page.evaluate(() => window.__fleet!.view().plan.direct.filter((x) => x.id !== "d:fcvm:3").map((x) => x.height));
  expect(new Set(others)).toEqual(new Set([72]));
  await expect(page.getByTestId("readings")).toContainText("Temperature");
  await expect(page.getByTestId("readings")).toContainText(/\d+ °C/);
  await expect(page.getByTestId("readings")).toContainText(/\d+ W/);
  // The restart before this test left a timeline measured by the coordinator.
  await expect(page.getByTestId("timeline")).toContainText("GPU reset and driver loaded");
  await expect(page.getByTestId("timeline-total")).toHaveText(/^\d\.\d s$/);
  await page.waitForTimeout(1000);
  const fe = await focusError(page, "fcvm:3");
  console.log(`opened VM: error ${fe.err.toFixed(2)} shown ${fe.shown}`);
  expect(fe.shown).toBe(`sync@${fe.T}`);
  expect(fe.err).toBeLessThan(6);
  await page.getByRole("button", { name: "Back to the wall" }).click();
  await expect(page.getByTestId("focus")).toHaveCount(0);
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
  const counters = async () => ((await mockState()).state as unknown as { counters: { restores_total: number } }).counters;
  const n0 = (await counters()).restores_total;
  await expect(page.getByTestId("counter")).toContainText("GPU VMs restored:");
  await expect(page.getByTestId("counter-value")).toHaveText(thousands(n0));
  await expect(page.getByTestId("counter-scrubs")).toHaveCount(0);
  await page.evaluate(() => window.__fleet!.client.send("relaunch", { tile: "fcvm:1" }));
  await expect(page.getByTestId("counter-value")).toHaveText(thousands(n0 + 1), { timeout: 10_000 });
  // The new total arrives highlighted: it ticks.
  expect(await page.getByTestId("counter-value").evaluate((el) => getComputedStyle(el).animationName)).toBe("tick");
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
  // Once the scrub has shipped, the page says so, and counts scrubs.
  await admin("/mock/schedule", { gpu_scrub: true });
  try {
    await expect(panel.locator("p").last()).toHaveText("GPU memory is cleared before a GPU moves to the next virtual machine.");
    await expect(page.getByTestId("counter-scrubs")).toHaveText(/^GPU memory scrubs: [\d,]+$/);
  } finally {
    await admin("/mock/schedule", { gpu_scrub: false });
  }
  await expect(panel.locator("p")).toHaveCount(3);
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
  await pa.evaluate(() => window.__fleet!.client.send("time_of_day", { v: 0.75 }));
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
  await pa.evaluate(() => window.__fleet!.client.send("time_of_day", { v: 0.42 }));
  await a.close();
  await b.close();
});

test("this viewer's own actions read as you", async ({ page }) => {
  await openWall(page, 15);
  await page.evaluate(() => window.__fleet!.client.send("time_of_day", { v: 0.6 }));
  await expect(page.getByTestId("feed")).toContainText("you set the time of day to 14:24");
  await page.evaluate(() => window.__fleet!.client.send("time_of_day", { v: 0.42 }));
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

test("over the bandwidth budget, small direct tiles ask for q=lo", async ({ page }) => {
  await page.addInitScript(() => window.localStorage.setItem("fleet-wall.mode", "direct"));
  // Under what 15 streams cost at their floor (9 Mbit/s), whatever the screen.
  await page.goto("/?debug=1&budget=5");
  await page.waitForFunction(() => window.__fleet !== undefined && window.__fleet.view().plan.direct.length > 0);
  const urls = await page.evaluate(() => window.__fleet!.view().plan.direct.map((d) => d.url!));
  expect(urls.filter((u) => u.includes("q=lo")).length).toBeGreaterThan(0);
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.bench?.tilesSynced ?? 0), { timeout: 30_000 }).toBeGreaterThanOrEqual(15);
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
  await expect(page.getByTestId("pause")).toHaveText("Resume");
  const t0 = await drawnT(page);
  await page.waitForTimeout(1200);
  expect(await drawnT(page)).toBe(t0);
  await page.getByTestId("pause").click();
  await expect.poll(() => drawnT(page)).toBeGreaterThan(t0!);
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
  await admin("/mock/fault", { reclaim: "mock-host-2" });
  await expect(page.getByTestId("banner-reclaimed")).toHaveText("AWS reclaimed an fcvm host. Its squares stay dark while the image is re-tiled onto the GPUs left.");
  await expect(page.getByTestId("dark-fcvm:5")).toHaveAttribute("data-kind", "reclaimed");
  await expect(says(page, "fcvm VM 5")).toHaveAttribute("aria-label", /host reclaimed by AWS, re-tiling/);
  // Its GPUs leave and the image is re-tiled onto the GPUs left: no dark places stay.
  await expect(says(page, "fcvm VM 5")).toHaveCount(0, { timeout: 15_000 });
  await expect(page.getByTestId("banner-reclaimed")).toHaveCount(0);
  await expect(page.getByTestId("restart-all")).toHaveText("Restart all 11 GPUs");
  await admin("/mock/fault", { replace_host: "mock-host-2" });
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
  await page.mouse.move(shot!.x + shot!.width / 2, shot!.y + shot!.height / 2);
  await page.mouse.down();
  for (let i = 1; i <= 8; i++) {
    await page.mouse.move(shot!.x + shot!.width / 2 + i * 12, shot!.y + shot!.height / 2);
    await page.waitForTimeout(25);
  }
  await page.mouse.up();
  await expect(page.getByTestId("own-latency")).toHaveText(/^input to pixels: \d+ ms$/, { timeout: 10_000 });
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
  await page.evaluate(() => window.__fleet!.client.send("time_of_day", { v: 0.5 }));
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.getState().version)).toBeGreaterThan(v0);
  expect(await page.evaluate(() => window.__fleet!.client.getState().connection)).toBe("open");
  await visibility.set(page, "visible");
  await expect.poll(subscribers, { timeout: 10_000 }).toBeGreaterThanOrEqual(15);
  await page.evaluate(() => window.__fleet!.client.send("time_of_day", { v: 0.42 }));
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
  await sleepNow();
  await expect(page.getByTestId("sleep-card")).toContainText("The GPUs are asleep");
  await expect(page.locator(".dark-tile")).toHaveCount(0);
  await expect(page.getByTestId("reel")).toHaveAttribute("data-state", "playing", { timeout: 15_000 });
  const t0 = await page.getByTestId("reel-video").evaluate((v) => (v as HTMLVideoElement).currentTime);
  await page.waitForTimeout(600);
  expect(await page.getByTestId("reel-video").evaluate((v) => (v as HTMLVideoElement).currentTime)).not.toBe(t0);
  expect(await page.getByTestId("reel-video").evaluate((v) => { const x = v as HTMLVideoElement; return [x.muted, x.loop, x.playsInline, x.textTracks[0]?.mode]; })).toEqual([true, true, true, "showing"]);
  await expect(page.getByTestId("reel-badge")).toHaveText(`Recorded ${await reelDate()}. The live wall is asleep; it wakes when someone watches.`);
  await expect(page.getByTestId("scoreboard")).toHaveAttribute("aria-label", /· last measured \d+ \w+ \d{4}, \d\d:\d\d UTC$/);
  await expect(page.getByTestId("restart-note")).toHaveText("works once the GPUs are awake");
  await expect(page.getByTestId("own-note")).toHaveText("works once the GPUs are awake");
  await expect(page.getByTestId("restart-all")).toBeDisabled();

  // Waking: a live clock since the wake began and each phase as the hosts report it.
  await page.evaluate(() => window.__fleet!.client.traceStart());
  await admin("/mock/power", { wake: true });
  await expect(page.getByTestId("sleep-card")).toContainText("Waking up the GPUs");
  await expect(page.getByTestId("reel-badge")).toHaveText(`Recorded ${await reelDate()}. The live wall is waking up.`);
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
  const trace = await page.evaluate(() => window.__fleet!.client.traceStop());
  expect(trace.filter((e) => e.T !== null && e.missing > 0 && !e.poster)).toEqual([]);
  // The wake, measured once.
  await expect(page.getByTestId("wake-stat")).toHaveText(/^Last wake: 15 GPUs from zero in 0:0\d \(\d+ \w+ \d{4}, \d\d:\d\d UTC\)$/);
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
    await expect(page.getByTestId("sleep-card")).toContainText("The GPUs are asleep");
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

test("phone: the image full width, the two bars, one button, and a hint to turn the phone", async ({ browser }) => {
  const ctx = await browser.newContext({ ...devices["iPhone 13"], deviceScaleFactor: 1 });
  const page = await ctx.newPage();
  await page.goto("/?debug=1");
  await page.waitForFunction(() => window.__fleet !== undefined && window.__fleet.view().layout !== null);
  const vw = page.viewportSize()!.width;
  const img = await page.evaluate(() => window.__fleet!.view().layout!.image);
  expect(img.w).toBe(vw);
  await expect(page.getByTestId("scoreboard")).toBeVisible();
  await expect(page.getByTestId("bar-fcvm")).toBeVisible();
  await expect(page.getByTestId("bar-ec2")).toBeVisible();
  // One button; Play is a text link under it.
  await expect(page.locator(".phone .btn:visible")).toHaveCount(1);
  await page.getByTestId("phone-play").click();
  await expect(page.getByTestId("drawer")).toBeVisible();
  await page.getByTestId("text-style-landmark").click();
  await expect(page.getByRole("radio", { name: "Landmark" })).toBeChecked();
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);
  await page.getByRole("button", { name: "Close" }).click();
  await expect(page.getByTestId("restart-all")).toBeVisible();
  await expect(page.getByTestId("rotate-hint")).toHaveText("Turn your phone sideways for a bigger picture.");
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
  expect(overflow).toBeLessThanOrEqual(0);
  await ctx.close();
});
