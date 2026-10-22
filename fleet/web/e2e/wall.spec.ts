import { expect, test, devices, type Page } from "@playwright/test";
import { COORD, focusError, mockState, openWall, seamReport, setSkew } from "./helpers.ts";

const view = (page: Page) =>
  page.evaluate(() => {
    const v = window.__fleet!.view();
    return { version: v.version, tiles: v.tiles.map((t) => ({ id: t.id, name: t.name, half: t.half, box: t.box, dark: t.dark })), image: v.layout?.image ?? null };
  });

// Mean absolute error, 0..255 per channel, at smooth points either side of each
// boundary, after one colour transform fitted over all of them (browsers convert
// YUV to RGB differently: before the fit, correct tiles read 1.0 in Chromium on
// arm64, 2.7 to 3.1 in WebKit and up to 5.1 in Chromium on CI's x86 runner).
// After it, correct draws at the common t measured 0.9 to 1.5 at worst over 16
// tiles in Chromium and WebKit; a tile drawn 0.6 s off its stamp 10.
const TILE_ERR_MAX = 3;
const ACROSS_ERR_MAX = 3;

test.describe.configure({ mode: "serial" });

test("the mosaic draws 16 tiles, 15 of them live at one common t", { tag: "@engines" }, async ({ page }) => {
  await openWall(page, 15);
  const v = await view(page);
  expect(v.tiles).toHaveLength(16);
  expect(v.tiles.filter((t) => t.box !== null)).toHaveLength(16);
  const bench = await page.evaluate(() => window.__fleet!.client.bench!);
  expect(bench.tilesSynced).toBe(15);
  expect(bench.tilesBehind).toBe(0);
  await expect(page.getByTestId("dark-ec2:8")).toContainText("waiting for capacity");
});

test("no seam: pixels on both sides of every boundary match the picture at the drawn t", { tag: "@engines" }, async ({ page }) => {
  await openWall(page, 15);
  const r = await seamReport(page);
  console.log(`seams: T=${r.T} samples=${r.samples} worst tile ${Math.max(...Object.values(r.perTile)).toFixed(2)} worst boundary ${Math.max(...r.boundaries.map((b) => b.err)).toFixed(2)}`);
  expect(r.samples).toBeGreaterThan(100);
  // Every live tile was drawn from its frame at exactly T.
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
    // slot-2 renders the tile fcvm:2.
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

test("hovering the image fades in every seam, outlines the tile and labels it", { tag: "@engines" }, async ({ page }) => {
  await openWall(page, 15);
  const v = await view(page);
  await expect(page.getByTestId("seams")).toHaveAttribute("data-visible", "0");
  const t = v.tiles.find((x) => x.name === "ec2-3")!;
  await page.mouse.move(t.box!.x + t.box!.w / 2, t.box!.y + t.box!.h / 2);
  await expect(page.getByTestId("seams")).toHaveAttribute("data-visible", "1");
  await expect(page.getByTestId("hover-outline")).toHaveAttribute("data-tile", "ec2-3");
  const outline = await page.getByTestId("hover-outline").evaluate((el) => {
    const r = el.getBoundingClientRect();
    return { x: r.x, y: r.y, w: r.width, h: r.height, color: getComputedStyle(el).borderTopColor };
  });
  expect(outline).toMatchObject({ x: t.box!.x, y: t.box!.y, w: t.box!.w, h: t.box!.h, color: "rgb(245, 165, 36)" });
  const label = page.getByTestId("hover-label");
  await expect(label).toContainText("ec2-3");
  await expect(label).toContainText("EC2");
  await expect(label).toContainText("Tesla T4");
  await expect(label).toContainText("g4dn.xlarge");
  await expect(label).toContainText(/no relaunch yet|last relaunch \d/);
  // The label covers none of the race and counter cards, for a tile right under one.
  const top = v.tiles.find((x) => x.name === "slot-1")!;
  await page.mouse.move(top.box!.x + top.box!.w / 2, top.box!.y + top.box!.h - 10);
  await expect(page.getByTestId("hover-outline")).toHaveAttribute("data-tile", "slot-1");
  const covered = await page.evaluate(() => {
    const l = document.querySelector(".hover-label")!.getBoundingClientRect();
    return [...document.querySelectorAll("[data-avoid]")].some((el) => {
      const c = el.getBoundingClientRect();
      return l.left < c.right && c.left < l.right && l.top < c.bottom && c.top < l.bottom;
    });
  });
  expect(covered).toBe(false);
  // The lines are drawn by the page, over the frames, not in them: the canvas under a seam is unchanged.
  const lines = await page.getByTestId("seams").locator("line").count();
  expect(lines).toBeGreaterThanOrEqual(10);
  // Off the image, onto the controls bar.
  await page.mouse.move(5, page.viewportSize()!.height - 5);
  await expect(page.getByTestId("seams")).toHaveAttribute("data-visible", "0");
  await expect(page.getByTestId("hover-outline")).toHaveCount(0);
});

test("keyboard: the tile grid takes focus, arrows move the outline, Enter opens, Escape returns", async ({ page }) => {
  await openWall(page, 15);
  const first = page.locator('.key-grid button[tabindex="0"]');
  await first.focus();
  await expect(page.getByTestId("hover-outline")).toHaveAttribute("data-tile", "slot-1");
  await expect(page.getByTestId("seams")).toHaveAttribute("data-visible", "1");
  await page.keyboard.press("ArrowRight");
  await expect(page.getByTestId("hover-outline")).toHaveAttribute("data-tile", "slot-2");
  await page.keyboard.press("ArrowRight");
  await expect(page.getByTestId("hover-outline")).toHaveAttribute("data-tile", "ec2-1");
  await page.keyboard.press("ArrowDown");
  await expect(page.getByTestId("hover-outline")).toHaveAttribute("data-tile", "ec2-3");
  await expect(page.getByTestId("hover-label")).toContainText("ec2-3");
  await page.keyboard.press("Enter");
  await expect(page.getByTestId("focus")).toBeVisible();
  await expect(page.getByRole("heading", { name: "ec2-3" })).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("focus")).toHaveCount(0);
  await expect(page.locator(".key-grid button:focus")).toHaveAttribute("aria-label", /^ec2-3,/);
});

test("opening a tile asks only that renderer for a taller stream of the same rectangle", async ({ page }) => {
  await openWall(page, 15);
  const before = await page.evaluate(() => window.__fleet!.client.getState().frames!.tiles["d:fcvm:3"]!.height);
  const v = await view(page);
  const t = v.tiles.find((x) => x.id === "fcvm:3")!;
  await page.mouse.click(t.box!.x + t.box!.w / 2, t.box!.y + t.box!.h / 2);
  await expect(page.getByTestId("focus")).toBeVisible();
  // The height its box takes on screen, which the page asks that renderer for.
  const wanted = await page.evaluate(() => window.__fleet!.view().plan.direct.find((x) => x.id === "d:fcvm:3")!.height);
  expect(wanted).toBeGreaterThan(before * 1.5);
  await expect
    .poll(() => page.evaluate(() => window.__fleet!.client.getState().frames!.tiles["d:fcvm:3"]!.height), { timeout: 15_000 })
    .toBe(wanted);
  const after = await page.evaluate(() => window.__fleet!.client.getState().frames!.tiles);
  expect(after["d:fcvm:3"]!.height).toBeGreaterThan(before);
  // The others keep streaming (for an instant return) at the smallest height.
  const others = await page.evaluate(() => window.__fleet!.view().plan.direct.filter((x) => x.id !== "d:fcvm:3").map((x) => x.height));
  expect(new Set(others)).toEqual(new Set([72]));
  // Same rectangle: the drawn frame keeps the tile's aspect.
  expect(after["d:fcvm:3"]!.width / after["d:fcvm:3"]!.height).toBeCloseTo(t.box!.w / t.box!.h, 1);
  await expect(page.getByTestId("focus-stream")).toContainText(new RegExp(`^${wanted}p`));
  // And it is the picture: the tall stream matches the pattern at the drawn t.
  await page.waitForTimeout(1000);
  const fe = await focusError(page, "fcvm:3");
  console.log(`opened tile: error ${fe.err.toFixed(2)} shown ${fe.shown}`);
  expect(fe.shown).toBe(`sync@${fe.T}`);
  expect(fe.err).toBeLessThan(6);
  await page.getByRole("button", { name: "Back to the wall" }).click();
  await expect(page.getByTestId("focus")).toHaveCount(0);
});

test("a repeated intent id applies once, and both receipts are the same", async ({ page }) => {
  await openWall(page, 15);
  const id = await page.evaluate(() => {
    const id = crypto.randomUUID();
    window.__fleet!.client.send("word", { text: "twice" }, id);
    window.__fleet!.client.resend(id);
    return id;
  });
  await expect(page.getByTestId("word-status")).toHaveText("“twice” is in the scene");
  // Shown in the hint stack on the image, where it cannot collide with the hint.
  await expect(page.locator(".hint [data-testid=word-status]")).toHaveCount(1);
  const applied = (await (await fetch(`${COORD}/mock/applied?id=${id}`)).json()) as { count: number };
  expect(applied.count).toBe(1);
  const st = await mockState();
  expect(st.state.scene.words.filter((w) => w.text === "twice")).toHaveLength(1);
  const notices = await page.evaluate((i) => window.__fleet!.client.getState().notices.filter((n) => n.id === i).length, id);
  expect(notices).toBe(1);
});

test("the text check refuses a word, and the page says so", async ({ page }) => {
  await openWall(page, 15);
  await page.getByLabel("Write a word into the world").fill("forbidden");
  await page.getByRole("button", { name: "Write" }).click();
  await expect(page.getByTestId("word-status")).toHaveText("Checking “forbidden”");
  await expect(page.getByTestId("word-status")).toHaveText("“forbidden” was not added: the text check refused it");
  const st = await mockState();
  expect(st.state.scene.words.some((w) => w.text === "forbidden")).toBe(false);
});

test("dragging steers the shared camera and shows input-to-pixel latency", async ({ page }) => {
  await openWall(page, 15);
  await page.mouse.move(400, 400);
  await page.mouse.down();
  for (let i = 1; i <= 15; i++) {
    await page.mouse.move(400 + i * 10, 400);
    await page.waitForTimeout(20);
  }
  await page.mouse.up();
  await expect(page.getByTestId("latency")).toHaveText(/^input to pixels: \d+ ms$/);
  const ms = Number((await page.getByTestId("latency").textContent())!.match(/(\d+) ms/)![1]);
  expect(ms).toBeGreaterThan(0);
  expect(ms).toBeLessThan(2000);
});

test("releasing a drag sends the movement since the last send", async ({ page }) => {
  await openWall(page, 15);
  const camX = async () => {
    const st = await mockState();
    const keys = st.pattern.camera as unknown as { x: number }[];
    return keys[keys.length - 1]!.x;
  };
  const x0 = await camX();
  const img = await page.evaluate(() => window.__fleet!.view().layout!.image);
  // Two moves inside one 50 ms send interval, then release at once.
  await page.mouse.move(400, 400);
  await page.mouse.down();
  await page.mouse.move(500, 400);
  await page.mouse.move(600, 400);
  await page.mouse.up();
  // 200 px of drag: x moves by 200/w of the image width, in world units.
  const want = x0 - (200 / img.w) * (16 / 9);
  await expect.poll(camX).toBeCloseTo(want, 6);
});

test("a dropped light lands in the world where it was clicked", async ({ page }) => {
  await openWall(page, 15);
  const img = await page.evaluate(() => window.__fleet!.view().layout!.image);
  const before = (await mockState()).pattern.lights.length;
  const [u, v] = [0.3, 0.4];
  await page.getByRole("button", { name: "Drop a light" }).click();
  await page.mouse.click(img.x + u * img.w, img.y + v * img.h);
  await expect.poll(async () => (await mockState()).pattern.lights.length).toBe(before + 1);
  const st = await mockState();
  // The page sent a world position (fleet/PROTOCOL.md: [x, altitude, z]) on the
  // ray through the click; the mock shows it where that position projects.
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
  await expect(pb.locator("#tod")).toHaveAttribute("aria-valuetext", "18:00");
  // Read both until they sit on one version, then compare what each accepted.
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

test("relaunch all: one clock, fcvm back first, a finish stamp for each half", async ({ page, browser }) => {
  await openWall(page, 15);
  const watcher = await (await browser.newContext()).newPage();
  await openWall(watcher, 15);
  await page.getByTestId("relaunch-all").click();
  await expect(page.getByTestId("relaunch-all")).toBeDisabled();
  await expect(page.getByTestId("race-clock")).toContainText("since Relaunch all");
  // The other viewer sees the same race.
  await expect(watcher.getByTestId("race-clock")).toContainText("since Relaunch all");
  await expect(page.getByTestId("stamp-fcvm")).toHaveText(/^all back at 0:0\d\.\d$/, { timeout: 10_000 });
  await expect(page.getByTestId("race-ec2")).toContainText("0 / 7 back");
  await expect(page.getByTestId("stamp-ec2")).toHaveText("still launching");
  await expect(page.getByTestId("stamp-ec2")).toHaveText(/^last relaunch all: \d+ s$/, { timeout: 30_000 });
  await expect(page.getByTestId("stamp-fcvm")).toHaveText(/^last relaunch all: \d\.\d s$/);
  const st = await mockState();
  const ra = (st.state as unknown as { relaunch_all: { started_at: number; fcvm_done_at: number; ec2_done_at: number } }).relaunch_all;
  expect(ra.fcvm_done_at - ra.started_at).toBeLessThan(ra.ec2_done_at - ra.started_at);
  await expect(page.getByTestId("relaunch-all")).toBeEnabled();
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.bench?.tilesSynced ?? 0), { timeout: 20_000 }).toBe(15);
  await watcher.context().close();
});

test("auto-relaunch is one shared setting: every viewer sees who set it and the countdown", async ({ page, browser }) => {
  await openWall(page, 15);
  const other = await (await browser.newContext()).newPage();
  await openWall(other, 15);
  const before = await page.evaluate(() => window.__fleet!.client.getState().accepted!.actions.length);
  await page.getByLabel("Auto-relaunch, shared").selectOption({ label: "Every 2 minutes" });
  // The coordinator's own record of who set it, once the snapshot carries it.
  let setter = "";
  await expect
    .poll(async () => {
      setter = await page.evaluate((n) => {
        const a = window.__fleet!.client.getState().accepted!.actions.slice(n).filter((x) => x.name === "set_auto_relaunch" && x.outcome !== "refused").pop();
        return a?.by ?? "";
      }, before);
      return setter;
    })
    .not.toBe("");
  await expect(other.getByLabel("Auto-relaunch, shared")).toHaveValue("120");
  await expect(other.getByTestId("auto-note")).toHaveText(new RegExp(`^set by ${setter} · next in 1:\\d\\d$`));
  await other.getByLabel("Auto-relaunch, shared").selectOption({ label: "Off" });
  await expect(page.getByLabel("Auto-relaunch, shared")).toHaveValue("off");
  await expect(page.getByTestId("auto-note")).toHaveText("");
  await other.context().close();
});

test("switching modes never shows a region without a frame, nor a t going back, both ways", { tag: "@engines" }, async ({ page }) => {
  await openWall(page, 15, "direct");
  await page.evaluate(() => window.__fleet!.client.traceStart());
  await page.getByTestId("mode-aggregated").click();
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.getState().ui.shownMode), { timeout: 20_000 }).toBe("aggregated");
  await page.waitForTimeout(1500);
  await page.getByTestId("mode-direct").click();
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.getState().ui.shownMode), { timeout: 20_000 }).toBe("direct");
  await page.waitForTimeout(1500);
  const trace = await page.evaluate(() => window.__fleet!.client.traceStop());
  const modes = trace.map((e) => e.mode);
  // Both switches happened inside the trace.
  expect(modes.indexOf("aggregated")).toBeGreaterThan(0);
  expect(modes.lastIndexOf("direct")).toBeGreaterThan(modes.indexOf("aggregated"));
  expect(trace.filter((e) => e.missing > 0)).toEqual([]);
  for (let i = 1; i < trace.length; i++) expect(trace[i]!.T!, `draw ${i}`).toBeGreaterThanOrEqual(trace[i - 1]!.T!);
  // The direct streams were closed while aggregated was shown, and reopened.
  expect(await page.evaluate(() => Object.keys(window.__fleet!.client.getState().frames!.tiles).some((k) => k.startsWith("d:")))).toBe(true);
});

test("aggregated: no seam, and tile state drawn exactly as in direct mode", { tag: "@engines" }, async ({ page }) => {
  await openWall(page, 2, "aggregated");
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.getState().ui.shownMode)).toBe("aggregated");
  await page.waitForTimeout(1000);
  const r = await seamReport(page);
  console.log(`aggregated seams: T=${r.T} samples=${r.samples} worst tile ${Math.max(...Object.values(r.perTile)).toFixed(2)} worst boundary ${Math.max(...r.boundaries.map((b) => b.err)).toFixed(2)}`);
  for (const [id, sh] of Object.entries(r.shown)) if (sh !== "dark") expect(sh, id).toBe(`sync@${r.T}`);
  expect(Object.keys(r.shown).every((k) => k.startsWith("a:") || r.shown[k] === "dark")).toBe(true);
  for (const [id, e] of Object.entries(r.perTile)) expect(e, `tile ${id}`).toBeLessThan(TILE_ERR_MAX);
  for (const b of r.boundaries) expect(b.err, `${b.a} | ${b.b}`).toBeLessThan(ACROSS_ERR_MAX);
  // The waiting-for-capacity tile is dark and labelled from state, as in direct mode.
  await expect(page.getByTestId("dark-ec2:8")).toContainText("waiting for capacity");
  const box = (await view(page)).tiles.find((t) => t.id === "ec2:8")!.box!;
  const res = await page.evaluate((p) => window.__fleet!.client.capture(p), [{ x: box.x + box.w / 2, y: box.y + box.h / 3 }]);
  expect(res.pixels[0]!.slice(0, 3)).toEqual([6, 8, 11]);
  // Hovering outlines and labels the tile, from state.
  const t = (await view(page)).tiles.find((x) => x.name === "slot-3")!;
  await page.mouse.move(t.box!.x + t.box!.w / 2, t.box!.y + t.box!.h / 2);
  await expect(page.getByTestId("hover-outline")).toHaveAttribute("data-tile", "slot-3");
  await expect(page.getByTestId("hover-label")).toContainText("Tesla T4");
});

test("this viewer's own actions read as you", async ({ page }) => {
  await openWall(page, 15);
  await page.evaluate(() => window.__fleet!.client.send("time_of_day", { v: 0.6 }));
  await expect(page.locator(".ticker")).toContainText("you set the time of day to 14:24");
  await page.evaluate(() => window.__fleet!.client.send("time_of_day", { v: 0.42 }));
});

test("over the bandwidth budget, small direct tiles ask for q=lo", async ({ page }) => {
  await page.addInitScript(() => window.localStorage.setItem("fleet-wall.mode", "direct"));
  await page.goto("/?debug=1&budget=10");
  await page.waitForFunction(() => window.__fleet !== undefined && window.__fleet.view().plan.direct.length > 0);
  const urls = await page.evaluate(() => window.__fleet!.view().plan.direct.map((d) => d.url!));
  expect(urls.filter((u) => u.includes("q=lo")).length).toBeGreaterThan(0);
  // The mock renderers take q, and the tiles still draw at one t.
  await expect.poll(() => page.evaluate(() => window.__fleet!.client.bench?.tilesSynced ?? 0), { timeout: 30_000 }).toBeGreaterThanOrEqual(15);
});

test("touch: a long press shows the pressed tile's seams and label", async ({ browser }) => {
  // A touch screen at one device pixel per CSS pixel: the mock renders every
  // pixel in JavaScript, so a 2.6x screen would ask it for more than it can draw.
  const ctx = await browser.newContext({ hasTouch: true, isMobile: true, viewport: { width: 1000, height: 700 }, deviceScaleFactor: 1 });
  const page = await ctx.newPage();
  await openWall(page, 15);
  const v = await view(page);
  const t = v.tiles.find((x) => x.name === "slot-5")!;
  const cdp = await ctx.newCDPSession(page);
  const x = t.box!.x + t.box!.w / 2;
  const y = t.box!.y + t.box!.h / 2;
  await cdp.send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: [{ x, y }] });
  await page.waitForTimeout(700);
  await expect(page.getByTestId("seams")).toHaveAttribute("data-visible", "1");
  await expect(page.getByTestId("hover-outline")).toHaveAttribute("data-tile", "slot-5");
  await expect(page.getByTestId("hover-label")).toContainText("slot-5");
  await cdp.send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
  await expect(page.getByTestId("seams")).toHaveAttribute("data-visible", "0");
  // A long press does not open the tile.
  await expect(page.getByTestId("focus")).toHaveCount(0);
  await ctx.close();
});

test("phone layout", async ({ browser }) => {
  const ctx = await browser.newContext({ ...devices["iPhone 13"] });
  const page = await ctx.newPage();
  await openWall(page, 15);
  await expect(page.getByRole("button", { name: "Relaunch all" })).toBeVisible();
  await expect(page.locator(".tile-card")).toHaveCount(16);
  await expect(page.getByLabel("Auto-relaunch, shared")).toBeVisible();
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
  expect(overflow).toBeLessThanOrEqual(0);
  await ctx.close();
});
