// Minecraft mode (`?mode=minecraft`) against the mock fleet with four servers
// (playwright.minecraft.config.ts), on a desktop screen and on a phone: each tile draws its world
// from its own view socket, what each tile and the header say, reset, fork, taking a server over
// and driving its bot from the keyboard (or the on-screen buttons on a phone) with the time to the
// first change, and giving it back.

import { expect, test, type Browser, type Page, type TestInfo } from "@playwright/test";
import { COORD, admin, mockState } from "./helpers.ts";

const SLOTS = ["mc-1", "mc-2", "mc-3", "mc-4"];
const SERVER = "Pumpkin 3ea7d281 (Java Edition 26.3)";

type MockTile = { id: string; slot: string; phase: string; forked_from: string | null; borrowed: { session: string; viewer: string } | null; last_relaunch: { by: string; clicked_at: number; first_frame_at: number | null } };
type MockMc = { last_fork: { from: string; to: string } | null; tiles: MockTile[] };
const mockMc = async () => ((await mockState()).state as unknown as { minecraft: MockMc }).minecraft;
const record = async () =>
  (await (await fetch(`${COORD}/mock/minecraft-record`)).json()) as {
    intents: { name: string; outcome: string; args: Record<string, unknown> }[];
    drives: { slot: string; viewer: string; msg: Record<string, unknown>; ok: boolean; reason: string | null }[];
  };
const seconds = (text: string | null) => Number(/(\d+\.\d+) s/.exec(text ?? "")![1]);
const view = (page: Page) => page.evaluate(() => window.__minecraft!.view());
const tilesNow = (page: Page) => page.evaluate(() => window.__minecraft!.tiles.debug());

/** Open the page and wait until every running server's world has come over its view socket. */
async function openMinecraft(page: Page, maps = SLOTS.length): Promise<void> {
  await page.goto("/?mode=minecraft&debug=1");
  await page.waitForFunction(() => window.__minecraft !== undefined);
  await expect
    .poll(async () => Object.values(await tilesNow(page)).filter((t) => t.open && t.worldSeq !== null).length, { timeout: 20_000 })
    .toBeGreaterThanOrEqual(maps);
}

async function viewer(browser: Browser): Promise<Page> {
  const page = await (await browser.newContext({ viewport: { width: 1024, height: 640 } })).newPage();
  await openMinecraft(page);
  return page;
}

/**
 * How much of a tile's picture is not the page's background (#06080b), and how many colours it
 * holds: its rectangle of the 3D view's one WebGL canvas, or its own canvas with the flat map.
 */
async function drawnShare(page: Page, slot: string): Promise<{ share: number; colours: number }> {
  return page.getByTestId(`mc-canvas-${slot}`).evaluate((c: HTMLCanvasElement) => {
    const gl = document.querySelector<HTMLCanvasElement>('[data-renderer="3d"] [data-testid="mc-gl"]');
    let d: Uint8ClampedArray;
    if (gl) {
      // with ?debug=1 the WebGL canvas keeps its drawing buffer, so it can be copied out
      const r = c.getBoundingClientRect();
      const g = gl.getBoundingClientRect();
      const k = gl.width / g.width;
      const copy = document.createElement("canvas");
      copy.width = Math.max(1, Math.round(r.width * k));
      copy.height = Math.max(1, Math.round(r.height * k));
      const ctx = copy.getContext("2d")!;
      ctx.drawImage(gl, Math.round((r.left - g.left) * k), Math.round((r.top - g.top) * k), copy.width, copy.height, 0, 0, copy.width, copy.height);
      d = ctx.getImageData(0, 0, copy.width, copy.height).data;
    } else {
      d = c.getContext("2d")!.getImageData(0, 0, c.width, c.height).data;
    }
    let drawn = 0;
    const colours = new Set<number>();
    for (let i = 0; i < d.length; i += 4) {
      if (Math.abs(d[i]! - 6) + Math.abs(d[i + 1]! - 8) + Math.abs(d[i + 2]! - 11) > 12) drawn++;
      if (i % 64 === 0) colours.add((d[i]! << 16) | (d[i + 1]! << 8) | d[i + 2]!);
    }
    return { share: drawn / (d.length / 4), colours: colours.size };
  });
}

const stageNow = (page: Page) => page.evaluate(() => window.__minecraft!.stage());
const renderer = (page: Page) => page.getByTestId("mc-layer").getAttribute("data-renderer");

/** The driven bot on a tile, as the page's own view socket last heard it. */
async function drivenBot(page: Page, id: string): Promise<{ x: number; z: number; yaw: number } | null> {
  const t = (await tilesNow(page))[id];
  return t?.players.find((p) => p.driven) ?? null;
}

const isPhone = (info: TestInfo) => info.project.name === "phone";

test.describe.configure({ mode: "serial" });

test.beforeEach(async () => {
  await admin("/mock/minecraft", { release_all: true });
  await expect.poll(async () => (await mockMc()).tiles.every((t) => t.phase === "running" && t.borrowed === null), { timeout: 15_000 }).toBe(true);
});

test("each tile draws its world from its own view socket, and says its slot, ticks per second and time to first chunk", async ({ page }, info) => {
  await openMinecraft(page);
  const v = await view(page);
  expect(v.title).toBe("Minecraft");
  for (const slot of SLOTS) {
    // the world is drawn, in many colours, over nearly the whole tile
    await expect.poll(async () => (await drawnShare(page, slot)).share, { message: slot }).toBeGreaterThan(0.9);
    expect((await drawnShare(page, slot)).colours, slot).toBeGreaterThan(8);
    await expect(page.getByTestId(`mc-slot-${slot}`)).toHaveText(slot);
    await expect(page.getByTestId(`mc-tps-${slot}`)).toHaveText(/^\d+\.\d TPS$/);
    await expect(page.getByTestId(`mc-up-in-${slot}`)).toHaveText(/^Up in \d+\.\d\d s$/);
    await expect(page.getByTestId(`mc-tile-${slot}`)).toBeInViewport();
    for (const b of ["reset", "fork", "take"]) await expect(page.getByTestId(`mc-${b}-${slot}`)).toBeEnabled();
  }
  // each square, and the page does not scroll sideways
  for (const c of v.cards) expect(c.box!.w).toBeCloseTo(c.box!.h, 5);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  // three bots on every tile
  for (const t of Object.values(await tilesNow(page))) expect(t.players.filter((p) => p.name.startsWith("fcvm_bot"))).toHaveLength(3);
  // the header
  await expect(page.getByTestId("mc-up")).toHaveText("4 of 4 servers up");
  await expect(page.getByTestId("mc-server")).toHaveText(SERVER);
  await expect(page.getByTestId("mc-median")).toHaveText(/^Median restore to first chunk \d+\.\d\d s$/);
  await expect(page.getByTestId("mc-slowest")).toHaveText(/^Slowest \d+\.\d\d s$/);
  // nothing used yet is no line at all
  await expect(page.getByTestId("mc-last-fork")).toHaveCount(0);
  await expect(page.getByTestId("mc-last-reset-all")).toHaveCount(0);
  await expect(page.getByTestId("mc-stats")).not.toContainText("not used yet");
  await expect(page.getByTestId("mc-join-address")).toHaveText("mc-1.mc.example.test:25565");
  await expect(page.getByTestId("mc-join")).toContainText("Join with Java Edition 26.3:");
  // the join line is one line on a desktop screen
  if (!isPhone(info)) {
    expect(await page.getByTestId("mc-join").evaluate((el) => el.getBoundingClientRect().height)).toBeLessThan(40);
  }
  // its hint sits in that line, not over the tiles (it once took the wall's centred .hint style)
  const inRow = await page.getByTestId("mc-join-hint").evaluate((el) => {
    const h = el.getBoundingClientRect();
    const row = el.parentElement!.getBoundingClientRect();
    return getComputedStyle(el).position === "static" && h.top >= row.top - 1 && h.bottom <= row.bottom + 1;
  });
  expect(inRow).toBe(true);
  // the tab says which mode it is, and a mode that is not one keeps the wall's title
  await expect(page).toHaveTitle("Minecraft");
  const other = await page.context().newPage();
  await other.goto("/?mode=constructor");
  await expect(other).toHaveTitle("Every GPU, live");
  await other.close();
  await expect(page.getByTestId("mc-panel")).toHaveCount(0);
  const dir = process.env.FLEET_SCREENSHOT_DIR;
  if (dir) await page.screenshot({ path: `${dir}/minecraft-${isPhone(info) ? "phone" : "desktop"}.png` });
});

test("the wall has no servers in it, and the mode switch reaches Minecraft and back", async ({ page }) => {
  await page.goto("/?debug=1");
  await page.waitForFunction(() => window.__fleet !== undefined);
  await expect(page.getByTestId("minecraft")).toHaveCount(0);
  const ids = await page.evaluate(() => window.__fleet!.view().tiles.map((t) => t.id));
  expect(ids.some((id) => id.startsWith("mc"))).toBe(false);
  await expect(page.getByTestId("mode-minecraft")).toHaveAttribute("href", "/?debug=1&mode=minecraft");
  await page.getByTestId("mode-minecraft").click();
  await page.waitForFunction(() => window.__minecraft !== undefined);
  await expect(page.getByTestId("mode-minecraft")).toHaveAttribute("aria-current", "page");
  await page.getByTestId("mode-wall").click();
  await page.waitForFunction(() => window.__fleet !== undefined);
  await expect(page).toHaveURL(/\/\?debug=1$/);
});

test("Reset throws a server away: dark with a running timer, then back with a new Up in and its world drawn again", async ({ page }) => {
  await openMinecraft(page);
  await page.getByTestId("mc-reset-mc-2").click();
  await expect(page.getByTestId("mc-dark-mc-2")).toBeVisible();
  await expect(page.getByTestId("mc-timer-mc-2")).toHaveText(/^Restoring \d+\.\d s$/);
  await expect(page.getByTestId("mc-tps-mc-2")).toHaveText("-- TPS");
  await expect(page.getByTestId("mc-dark-mc-1")).toHaveCount(0);
  expect((await mockMc()).tiles[1]!.last_relaunch.by).toMatch(/^viewer:/);
  await expect(page.getByTestId("mc-tile-mc-2")).toHaveAttribute("data-phase", "live", { timeout: 15_000 });
  expect(seconds(await page.getByTestId("mc-up-in-mc-2").textContent())).toBeGreaterThanOrEqual(1.9);
  // the page opened a new socket to the new launch and drew its world
  await expect.poll(async () => (await tilesNow(page))["mc:2"]?.worldSeq ?? null, { timeout: 10_000 }).not.toBeNull();
  await expect.poll(async () => (await drawnShare(page, "mc-2")).share).toBeGreaterThan(0.9);
  await expect(page.getByTestId("mc-tps-mc-2")).toHaveText(/^\d+\.\d TPS$/);
  // a second reset at once is refused with the coordinator's reason
  await page.getByTestId("mc-reset-mc-3").click();
  await page.getByTestId("mc-reset-mc-4").click();
  await expect(page.getByTestId("mc-notice")).toHaveText("one reset per 2 s per client");
  await expect(page.getByTestId("mc-tile-mc-3")).toHaveAttribute("data-phase", "live", { timeout: 15_000 });
});

test("Fork copies a running server into the next one: the copy says Forked from, and the header gives the fork's times", async ({ page }) => {
  await openMinecraft(page);
  await page.getByTestId("mc-fork-mc-3").click();
  await expect(page.getByTestId("mc-timer-mc-4")).toHaveText(/^Forking \d+\.\d s$/);
  // the source keeps running
  await expect(page.getByTestId("mc-dark-mc-3")).toHaveCount(0);
  await expect(page.getByTestId("mc-tile-mc-4")).toHaveAttribute("data-phase", "live", { timeout: 15_000 });
  await expect(page.getByTestId("mc-forked-mc-4")).toHaveText(/^Forked from mc-3 in \d+\.\d\d s/);
  expect(seconds(await page.getByTestId("mc-forked-mc-4").textContent())).toBeGreaterThanOrEqual(2.5);
  await expect(page.getByTestId("mc-last-fork")).toHaveText(/^Last fork (just now|\d+ s ago): mc-3 to mc-4 in \d+\.\d\d s \(snapshot 0\.60 s, restore \d+\.\d\d s\)$/);
  expect((await mockMc()).tiles[3]).toMatchObject({ forked_from: "mc-3", last_relaunch: { by: "fork:mc-3" } });
  await expect.poll(async () => (await drawnShare(page, "mc-4")).share, { timeout: 10_000 }).toBeGreaterThan(0.9);
  // one fork per 10 s per client
  await page.getByTestId("mc-fork-mc-1").click();
  await expect(page.getByTestId("mc-notice")).toHaveText("one fork per 10 s per client");
});

test("taking a server over drives its first bot: a key moves it, the panel times the first change, and giving it back resets the server", async ({ page }, info) => {
  await openMinecraft(page);
  await page.getByTestId("mc-take-mc-2").click();
  const panel = page.getByTestId("mc-panel");
  await expect(panel).toBeVisible();
  await expect(page.getByTestId("mc-panel-title")).toHaveText("mc-2");
  await expect(page.getByTestId("mc-countdown")).toHaveText(/^Returns in \d+ s$/);
  await expect(page.getByTestId("mc-first-change")).toHaveText(/^Press a key/);
  await expect(page.getByTestId("mc-caption-mc-2")).toHaveAttribute("data-mine", "true");
  await expect(page.getByTestId("mc-waiting")).toHaveCount(0);
  const session = (await mockMc()).tiles[1]!.borrowed!.session;
  // the page's view socket marks the driven bot once the tile has a driver
  await expect.poll(async () => drivenBot(page, "mc:2")).not.toBeNull();
  const start = (await drivenBot(page, "mc:2"))!;
  if (isPhone(info)) {
    // a touch layout drives from the on-screen buttons: hold Forward
    await expect(page.getByTestId("mc-pad")).toBeVisible();
    const fwd = page.getByTestId("mc-pad-forward");
    await fwd.dispatchEvent("pointerdown", { pointerId: 1, pointerType: "touch", isPrimary: true });
    await expect.poll(async () => { const b = await drivenBot(page, "mc:2"); return Math.hypot(b!.x - start.x, b!.z - start.z); }, { timeout: 5000 }).toBeGreaterThan(1);
    await fwd.dispatchEvent("pointerup", { pointerId: 1, pointerType: "touch", isPrimary: true });
  } else {
    await expect(page.getByTestId("mc-pad")).toHaveCount(0);
    await page.keyboard.down("KeyW");
    await expect.poll(async () => { const b = await drivenBot(page, "mc:2"); return Math.hypot(b!.x - start.x, b!.z - start.z); }, { timeout: 5000 }).toBeGreaterThan(1);
    await page.keyboard.up("KeyW");
  }
  await expect(page.getByTestId("mc-first-change")).toHaveText(/^Key to first change: last \d+ ms, median \d+ ms \(1 key\)$/);
  const dir = process.env.FLEET_SCREENSHOT_DIR;
  if (dir) await page.screenshot({ path: `${dir}/minecraft-take-over-${isPhone(info) ? "phone" : "desktop"}.png` });
  const ms = Number(/last (\d+) ms/.exec((await page.getByTestId("mc-first-change").textContent())!)![1]);
  expect(ms).toBeGreaterThan(0);
  expect(ms).toBeLessThan(2000);
  // the bot stops when the key is up
  await page.waitForTimeout(300);
  const stopped = (await drivenBot(page, "mc:2"))!;
  await page.waitForTimeout(400);
  const still = (await drivenBot(page, "mc:2"))!;
  expect([still.x, still.z]).toEqual([stopped.x, stopped.z]);
  // turning: the arrows on a keyboard, the turn button on a phone
  if (isPhone(info)) {
    const turn = page.getByTestId("mc-pad-turn_right");
    await turn.dispatchEvent("pointerdown", { pointerId: 2, pointerType: "touch", isPrimary: true });
    await expect.poll(async () => Math.abs((await drivenBot(page, "mc:2"))!.yaw - still.yaw), { timeout: 5000 }).toBeGreaterThan(10);
    await turn.dispatchEvent("pointerup", { pointerId: 2, pointerType: "touch", isPrimary: true });
  } else {
    await page.keyboard.down("ArrowRight");
    await expect.poll(async () => Math.abs((await drivenBot(page, "mc:2"))!.yaw - still.yaw), { timeout: 5000 }).toBeGreaterThan(10);
    await page.keyboard.up("ArrowRight");
  }
  // every drive message reached mc-2 from this viewer and was accepted, with only the contract's fields
  const drives = (await record()).drives;
  expect(drives.length).toBeGreaterThan(2);
  for (const d of drives) {
    expect(d, JSON.stringify(d)).toMatchObject({ slot: "mc-2", ok: true });
    expect(Object.keys(d.msg).sort()).toEqual(["action", "keys", "pitch", "seq", "type", "yaw"]);
  }
  const seqs = drives.map((d) => d.msg["seq"] as number);
  expect(seqs).toEqual([...seqs].sort((a, b) => a - b));
  // give it back: Escape on a keyboard, the button on a phone
  if (isPhone(info)) await page.getByTestId("mc-give-back").click();
  else await page.keyboard.press("Escape");
  await expect(panel).toHaveCount(0);
  await expect.poll(async () => (await mockMc()).tiles[1]!.borrowed).toBeNull();
  expect((await mockMc()).tiles[1]!.last_relaunch.by).toBe(`return:${session}`);
  await expect(page.getByTestId("mc-tile-mc-2")).toHaveAttribute("data-phase", "live", { timeout: 15_000 });
});

test("a viewer with no IPv6 reaches every server over its IPv4 name: the worlds draw and a take-over drives; with IPv6, the tile's own name", async ({ page }) => {
  const family = () => page.evaluate(() => window.__minecraft!.client.getState().net.family);
  const hosts = async () => new Set(Object.values(await tilesNow(page)).map((t) => new URL(t.url).hostname));
  await admin("/mock/fault", { ipv6_down: true });
  try {
    await openMinecraft(page);
    expect(await family()).toBe("v4");
    expect(await hosts()).toEqual(new Set(["127.0.0.1"]));
    for (const slot of SLOTS) {
      await expect.poll(async () => (await drawnShare(page, slot)).share, { message: slot }).toBeGreaterThan(0.9);
      await expect(page.getByTestId(`mc-tps-${slot}`)).toHaveText(/^\d+\.\d TPS$/);
    }
    await page.getByTestId("mc-take-mc-3").click();
    await expect(page.getByTestId("mc-panel")).toBeVisible();
    await expect.poll(async () => drivenBot(page, "mc:3")).not.toBeNull();
    const start = (await drivenBot(page, "mc:3"))!;
    if (await page.getByTestId("mc-pad").count()) {
      const fwd = page.getByTestId("mc-pad-forward");
      await fwd.dispatchEvent("pointerdown", { pointerId: 1, pointerType: "touch", isPrimary: true });
      await expect.poll(async () => { const b = await drivenBot(page, "mc:3"); return Math.hypot(b!.x - start.x, b!.z - start.z); }, { timeout: 5000 }).toBeGreaterThan(1);
      await fwd.dispatchEvent("pointerup", { pointerId: 1, pointerType: "touch", isPrimary: true });
    } else {
      await page.keyboard.down("KeyW");
      await expect.poll(async () => { const b = await drivenBot(page, "mc:3"); return Math.hypot(b!.x - start.x, b!.z - start.z); }, { timeout: 5000 }).toBeGreaterThan(1);
      await page.keyboard.up("KeyW");
    }
    await expect(page.getByTestId("mc-first-change")).toHaveText(/^Key to first change: last \d+ ms, median \d+ ms \(1 key\)$/);
    await page.getByTestId("mc-give-back").click();
    await expect(page.getByTestId("mc-panel")).toHaveCount(0);
  } finally {
    await admin("/mock/fault", { ipv6_down: false });
  }
  // A new page load probes again: IPv6 works now, so every server is reached by its own name.
  await openMinecraft(page);
  expect(await family()).toBe("v6");
  expect(await hosts()).toEqual(new Set(["localhost"]));
});

test("a server whose view cannot be reached says so on its tile with a Retry, cannot be taken over, and draws once it can be reached", async ({ page }) => {
  await admin("/mock/fault", { minecraft_down: true });
  try {
    await page.goto("/?mode=minecraft&debug=1");
    await page.waitForFunction(() => window.__minecraft !== undefined);
    for (const slot of SLOTS) {
      await expect(page.getByTestId(`mc-stream-${slot}`)).toContainText(`Cannot reach ${slot}'s view. Trying again.`, { timeout: 10_000 });
      await expect(page.getByTestId(`mc-stream-retry-${slot}`)).toBeVisible();

      await expect(page.getByTestId(`mc-take-${slot}`)).toBeDisabled();
    }
    // The browser's own accessibility tree holds each Retry and each message: inside an element
    // with role img they would be presentational, and a screen reader would hear only the label.
    const cdp = await page.context().newCDPSession(page);
    const { nodes } = (await cdp.send("Accessibility.getFullAXTree")) as { nodes: { ignored: boolean; role?: { value: string }; name?: { value: string } }[] };
    const exposed = nodes.filter((n) => !n.ignored);
    expect(exposed.filter((n) => n.role?.value === "button" && n.name?.value === "Retry")).toHaveLength(SLOTS.length);
    for (const slot of SLOTS) expect(exposed.some((n) => n.role?.value === "StaticText" && n.name?.value === `Cannot reach ${slot}'s view. Trying again.`), slot).toBe(true);
  } finally {
    await admin("/mock/fault", { minecraft_down: false });
  }
  await page.getByTestId("mc-stream-retry-mc-1").click();
  await expect(page.getByTestId("mc-stream-mc-1")).toHaveCount(0, { timeout: 10_000 });
  await expect.poll(async () => (await drawnShare(page, "mc-1")).share, { timeout: 10_000 }).toBeGreaterThan(0.9);
  await expect(page.getByTestId("mc-take-mc-1")).toBeEnabled();
});

test("a handshake that never answers is given up after the deadline, and Retry starts a fresh one at once", async ({ page }) => {
  await admin("/mock/fault", { minecraft_hang: true });
  try {
    await page.goto("/?mode=minecraft&debug=1");
    await page.waitForFunction(() => window.__minecraft !== undefined);
    await expect(page.getByTestId("mc-stream-mc-1")).toContainText("Cannot reach mc-1's view. Trying again.", { timeout: 15_000 });
    // Retry under the same fault starts over: the tile says it is connecting again, without Retry
    await page.getByTestId("mc-stream-retry-mc-1").click();
    await expect(page.getByTestId("mc-stream-mc-1")).toContainText("Connecting to mc-1.", { timeout: 1000 });
    await expect(page.getByTestId("mc-stream-retry-mc-1")).toHaveCount(0);
    await expect(page.getByTestId("mc-stream-mc-1")).toContainText("Cannot reach mc-1's view. Trying again.", { timeout: 15_000 });
  } finally {
    await admin("/mock/fault", { minecraft_hang: false });
  }
  await page.getByTestId("mc-stream-retry-mc-1").click();
  await expect(page.getByTestId("mc-stream-mc-1")).toHaveCount(0, { timeout: 2000 });
  await expect.poll(async () => (await drawnShare(page, "mc-1")).share, { timeout: 5000 }).toBeGreaterThan(0.9);
});

test("the held server is the large view, with the others off the stage, and Give back closes the panel at once", async ({ page }, info) => {
  await openMinecraft(page);
  const small = (await page.getByTestId("mc-tile-mc-2").boundingBox())!;
  await page.getByTestId("mc-take-mc-2").click();
  await expect(page.getByTestId("mc-panel")).toBeVisible();
  const big = (await page.getByTestId("mc-tile-mc-2").boundingBox())!;
  // on a desktop it fills the room beside the panel; on a phone the half of the screen above it
  expect(big.width).toBeGreaterThan((isPhone(info) ? 1.05 : 1.3) * small.width);
  for (const slot of ["mc-1", "mc-3", "mc-4"]) await expect(page.getByTestId(`mc-tile-${slot}`)).toHaveCount(0);
  // the panel does not cover the held view
  const sheet = (await page.getByTestId("mc-panel").boundingBox())!;
  const overlap = !(big.x + big.width <= sheet.x || sheet.x + sheet.width <= big.x || big.y + big.height <= sheet.y || sheet.y + sheet.height <= big.y);
  expect(overlap).toBe(false);
  await page.getByTestId("mc-give-back").click();
  await expect(page.getByTestId("mc-panel")).toHaveCount(0, { timeout: 150 });
  for (const slot of SLOTS) await expect(page.getByTestId(`mc-tile-${slot}`)).toHaveCount(1);
});

test("a place and a dig change the world on the tile, timed to the delta that shows them", async ({ page }, info) => {
  await openMinecraft(page);
  await page.getByTestId("mc-take-mc-1").click();
  await expect(page.getByTestId("mc-panel")).toBeVisible();
  await expect.poll(async () => drivenBot(page, "mc:1")).not.toBeNull();
  // the keyboard's E and Q, or a phone's Place and Break buttons
  const act = async (key: string, input: string) => {
    if (!isPhone(info)) return page.keyboard.press(key);
    const b = page.getByTestId(`mc-pad-${input}`);
    await b.dispatchEvent("pointerdown", { pointerId: 3, pointerType: "touch", isPrimary: true });
    await b.dispatchEvent("pointerup", { pointerId: 3, pointerType: "touch", isPrimary: true });
  };
  await expect.poll(async () => (await drawnShare(page, "mc-1")).share).toBeGreaterThan(0.9);
  await act("KeyE", "place");
  await expect(page.getByTestId("mc-first-change")).toHaveText(/^Key to first change: last \d+ ms, median \d+ ms \(1 key\)$/, { timeout: 5000 });
  await expect.poll(async () => (await drawnShare(page, "mc-1")).share).toBeGreaterThan(0.9);
  await act("KeyQ", "dig");
  await expect(page.getByTestId("mc-first-change")).toHaveText(/\(2 keys\)$/, { timeout: 5000 });
  const actions = (await record()).drives.map((d) => d.msg["action"]).filter((a) => a !== null);
  expect(actions).toEqual(["place", "dig"]);
  await page.getByTestId("mc-give-back").click();
  await expect(page.getByTestId("mc-panel")).toHaveCount(0);
});

test("a server lent to another viewer shows no panel and cannot be reset, forked into or taken", async ({ page, browser }) => {
  await openMinecraft(page);
  const other = await viewer(browser);
  await page.getByTestId("mc-take-mc-4").click();
  await expect(page.getByTestId("mc-panel")).toBeVisible();
  await expect(other.getByTestId("mc-panel")).toHaveCount(0);
  await expect(other.getByTestId("mc-lent-mc-4")).toHaveText(/^Lent to another viewer until \d\d:\d\d:\d\d UTC$/);
  await expect(other.getByTestId("mc-take-mc-4")).toBeDisabled();
  await expect(other.getByTestId("mc-reset-mc-4")).toBeDisabled();
  await expect(other.getByTestId("mc-take-mc-1")).toBeEnabled();
  // the other viewer's keys go nowhere: it holds nothing, so its page sends no drive message
  await other.keyboard.down("KeyW");
  await other.waitForTimeout(400);
  await other.keyboard.up("KeyW");
  expect((await record()).drives.filter((d) => d.ok === false)).toEqual([]);
  await page.getByTestId("mc-give-back").click();
  await expect(other.getByTestId("mc-lent-mc-4")).toHaveCount(0);
  await expect(other.getByTestId("mc-take-mc-4")).toBeEnabled({ timeout: 15_000 });
  await other.context().close();
});

test("the page sends only the four intents and, on a view socket, its admission and drive messages", async ({ page }) => {
  const intents: string[] = [];
  const onView: Record<string, unknown>[] = [];
  page.on("websocket", (ws) =>
    ws.on("framesent", (f) => {
      if (typeof f.payload !== "string") return;
      const m = JSON.parse(f.payload) as Record<string, unknown>;
      if (ws.url().endsWith("/view")) onView.push(m);
      else if (m["type"] === "intent") intents.push(m["name"] as string);
    }),
  );
  await openMinecraft(page);
  await page.getByTestId("mc-take-mc-3").click();
  await expect(page.getByTestId("mc-panel")).toBeVisible();
  await expect.poll(async () => drivenBot(page, "mc:3")).not.toBeNull();
  await page.keyboard.type("say hello");
  await page.keyboard.press("Enter");
  await page.keyboard.press("KeyD");
  await page.getByTestId("mc-give-back").click();
  await expect(page.getByTestId("mc-panel")).toHaveCount(0);
  await page.waitForTimeout(500);
  expect(intents).toEqual(["claim_minecraft", "release_minecraft"]);
  for (const m of onView) {
    const keys = Object.keys(m).sort();
    if (keys.length === 1) expect(keys).toEqual(["admit"]);
    else {
      expect(keys).toEqual(["action", "keys", "pitch", "seq", "type", "yaw"]);
      for (const k of m["keys"] as string[]) expect(["forward", "back", "left", "right", "jump", "sprint"]).toContain(k);
    }
  }
  expect(onView.filter((m) => "admit" in m).length).toBeGreaterThanOrEqual(4);
  expect((await record()).intents.map((i) => [i.name, i.outcome])).toEqual([["claim_minecraft", "committed"], ["release_minecraft", "committed"]]);
});

test("the worlds are drawn in 3D on one WebGL canvas, each tile from its own moving camera, with the bots named", async ({ page }) => {
  await openMinecraft(page);
  expect(await renderer(page)).toBe("3d");
  await expect(page.getByTestId("mc-gl")).toHaveCount(1);
  // what is drawn from data and what is filled in: under the header where there is a line for it, else under an opened server
  await expect(page.getByTestId("mc-drawn")).toHaveText("Drawn from each column's top block; trunks are filled in, and so are the forest floor and lake beds of a server that does not send them.");
  if (isPhone(test.info())) await expect(page.getByTestId("mc-drawn")).toBeHidden();
  else await expect(page.getByTestId("mc-drawn")).toBeVisible();
  await expect.poll(async () => (await stageNow(page))!.held).toBe(SLOTS.length);
  const st = (await stageNow(page))!;
  // a renderer that draws on the CPU starts at the guard's last level, and every tile says the view is reduced
  if (st.software) {
    expect(st.level).toBeGreaterThan(0);
    for (const slot of SLOTS) {
      await expect(page.getByTestId(`mc-note-${slot}`)).toHaveText("Reduced detail");
      await expect(page.getByTestId(`mc-desc-${slot}`)).toHaveText(/, reduced detail$/);
    }
  }
  // six draw calls a world
  expect(st.calls).toBe(SLOTS.length * 6);
  // no two tiles look from the same place
  const eyes = Object.values(st.shots).map((s) => s.eye.map((v) => v.toFixed(1)).join());
  expect(new Set(eyes).size).toBe(SLOTS.length);
  // the camera flies: with the frame-time guard off (this renderer is software), a tile's eye moves
  await page.evaluate(() => window.__minecraft!.setBudget(false));
  // at full detail nothing is said
  for (const slot of SLOTS) await expect(page.getByTestId(`mc-note-${slot}`)).toHaveCount(0);
  const eye = async () => (await stageNow(page))!.shots["mc:1"]!.eye.join();
  const first = await eye();
  await expect.poll(eye).not.toBe(first);
  await page.evaluate(() => window.__minecraft!.setBudget(true));
  // a bot that shows is named on its tile's own canvas, over the picture
  await expect
    .poll(
      async () => {
        let named = 0;
        for (const slot of SLOTS) {
          named += await page.getByTestId(`mc-canvas-${slot}`).evaluate((c: HTMLCanvasElement) => {
            const d = c.getContext("2d")!.getImageData(0, 0, c.width, c.height).data;
            for (let i = 3; i < d.length; i += 4) if (d[i]! > 0) return 1;
            return 0;
          });
        }
        return named;
      },
      { timeout: 30_000 },
    )
    .toBeGreaterThan(0);
  // a hidden tab holds nothing on the GPU, and gets it back when it is shown again
  const show = (state: string) =>
    page.evaluate((v) => {
      Object.defineProperty(document, "visibilityState", { configurable: true, get: () => v });
      document.dispatchEvent(new Event("visibilitychange"));
    }, state);
  await show("hidden");
  await expect.poll(async () => (await stageNow(page))!.held).toBe(0);
  await show("visible");
  await expect.poll(async () => (await stageNow(page))!.held).toBe(SLOTS.length);
  const dir = process.env.FLEET_SCREENSHOT_DIR;
  if (dir) await page.screenshot({ path: `${dir}/minecraft-3d-${test.info().project.name}.png` });
});

test("a browser with no WebGL2 gets the flat map, and every tile says so", async ({ browser }, info) => {
  const context = await browser.newContext(info.project.use);
  await context.addInitScript(() => {
    const real = HTMLCanvasElement.prototype.getContext;
    HTMLCanvasElement.prototype.getContext = function (this: HTMLCanvasElement, kind: string, ...rest: unknown[]) {
      return kind === "webgl2" ? null : (real as (...a: unknown[]) => unknown).call(this, kind, ...rest);
    } as typeof real;
  });
  const page = await context.newPage();
  await openMinecraft(page);
  expect(await renderer(page)).toBe("flat");
  await expect(page.getByTestId("mc-gl")).toHaveCount(0);
  expect(await stageNow(page)).toBeNull();
  await expect(page.getByTestId("mc-drawn")).toHaveCount(0);
  for (const slot of SLOTS) {
    await expect(page.getByTestId(`mc-note-${slot}`)).toHaveText("Flat map: no WebGL2 in this browser.");
    await expect(page.getByTestId(`mc-desc-${slot}`)).toHaveText(new RegExp(`^${slot}, running.*, flat map$`));
    // the canvas is a button that says what it does; what it shows is its description
    await expect(page.getByTestId(`mc-canvas-${slot}`)).toHaveAttribute("aria-label", `Open ${slot}`);
    await expect(page.getByTestId(`mc-canvas-${slot}`)).toHaveAttribute("aria-describedby", `mc-desc-${slot}`);
    await expect.poll(async () => (await drawnShare(page, slot)).share, { message: slot }).toBeGreaterThan(0.9);
    expect((await drawnShare(page, slot)).colours, slot).toBeGreaterThan(8);
  }
  await context.close();
});

test("with reduced motion every tile is one still 3D view, drawn again only when its world changes", async ({ browser }, info) => {
  const context = await browser.newContext({ ...info.project.use, reducedMotion: "reduce" });
  const page = await context.newPage();
  await openMinecraft(page);
  expect(await renderer(page)).toBe("3d");
  await expect.poll(async () => Object.keys((await stageNow(page))!.shots).length).toBe(SLOTS.length);
  // the stage's count of frames drawn, with the page's own clock at that moment
  const drawn = () => page.evaluate(() => ({ st: window.__minecraft!.stage()!, at: performance.now() }));
  const { st: a, at: aAt } = await drawn();
  expect(Object.values(a.shots).map((s) => s.kind)).toEqual(SLOTS.map(() => "wide"));
  // the bots move and the page draws them again, from the same place
  const where = async () => JSON.stringify((await tilesNow(page))["mc:1"]!.players.map((p) => [p.x, p.z]));
  const before = await where();
  await expect.poll(where, { timeout: 30_000 }).not.toBe(before);
  await expect.poll(async () => (await stageNow(page))!.frames).toBeGreaterThan(a.frames);
  // long enough to count frames against time
  await expect.poll(async () => (await drawn()).at - aAt, { timeout: 10_000 }).toBeGreaterThan(3000);
  const { st: b, at: bAt } = await drawn();
  for (const id of Object.keys(a.shots)) expect(b.shots[id]!.eye, id).toEqual(a.shots[id]!.eye);
  // never more than four frames a second: the frames drawn over the page's own elapsed time
  // (the pacer lets a frame go 4 ms early, so 246 ms apart; one more for a frame at either end)
  expect(b.frames - a.frames).toBeLessThanOrEqual(Math.ceil((bAt - aAt) / 246) + 1);
  expect(b.frames - a.frames).toBeGreaterThan(0);
  for (const slot of SLOTS) expect((await drawnShare(page, slot)).share, slot).toBeGreaterThan(0.9);
  await context.close();
});

test("a fork's picture flies from its source to the copy's tile, and the copy then counts the columns that differ", async ({ page }) => {
  await openMinecraft(page);
  const src = (await page.getByTestId("mc-tile-mc-1").boundingBox())!;
  const dst = (await page.getByTestId("mc-tile-mc-2").boundingBox())!;
  await page.getByTestId("mc-fork-mc-1").click();
  const flight = page.getByTestId("mc-flight-mc-2");
  // the flight lasts as long as the fork (under 3 s here), so the page itself watches it
  const seen = await page.evaluate(async () => {
    const out: { x: number; w: number; text: string; flights: string[] }[] = [];
    const t0 = performance.now();
    while (performance.now() - t0 < 12_000) {
      const el = document.querySelector('[data-testid="mc-flight-mc-2"]');
      if (el) {
        const r = el.getBoundingClientRect();
        out.push({ x: r.x, w: r.width, text: el.textContent ?? "", flights: window.__minecraft!.stage()!.flights });
      } else if (out.length > 0) break;
      await new Promise((r) => setTimeout(r, 50));
    }
    return out;
  });
  expect(seen.length).toBeGreaterThan(3);
  expect(seen.some((f) => f.text.includes("copy of mc-1") && /Forking \d+\.\d s/.test(f.text))).toBe(true);
  expect(seen.every((f) => f.flights.join() === "mc:2")).toBe(true);
  // on its way: it left the source, is smaller than a tile in mid flight, and moves towards the copy's tile
  expect(seen.some((f) => f.x > src.x + 4 && f.x < dst.x && f.w < src.width * 0.95)).toBe(true);
  expect(seen.at(-1)!.x).toBeGreaterThan(seen[0]!.x);
  await expect(page.getByTestId("mc-tile-mc-2")).toHaveAttribute("data-phase", "live", { timeout: 15_000 });
  await expect(flight).toHaveCount(0);
  expect((await stageNow(page))!.flights).toEqual([]);
  // the copy starts as its source and the two then diverge as their bots build
  await expect(page.getByTestId("mc-diff-mc-2")).toHaveText(/^(same so far|\d+ columns? differs?)$/);
  await expect(page.getByTestId("mc-diff-mc-2")).toHaveText(/^\d+ columns? differs?$/, { timeout: 60_000 });
  await expect(page.getByTestId("mc-desc-mc-2")).toHaveText(/copy of mc-1, \d+ columns? differs?/);
  // resetting the source ends the comparison: it is another world now
  await page.getByTestId("mc-reset-mc-1").click();
  await expect(page.getByTestId("mc-diff-mc-2")).toHaveCount(0);
});

test("a click or Enter opens one server to the whole stage, and Escape or All servers brings the wall back", async ({ page }) => {
  await openMinecraft(page);
  const small = (await page.getByTestId("mc-tile-mc-3").boundingBox())!;
  await page.getByTestId("mc-canvas-mc-3").click();
  await expect(page.getByTestId("mc-close")).toBeVisible();
  // focus goes with the view: to All servers, and back to the tile when the wall returns
  await expect(page.getByTestId("mc-close")).toBeFocused();
  await expect(page.getByTestId("mc-about-mc-3")).toHaveText("Drawn from each column's top block; trunks are filled in, and so are the forest floor and lake beds of a server that does not send them.");
  for (const slot of ["mc-1", "mc-2", "mc-4"]) await expect(page.getByTestId(`mc-tile-${slot}`)).toHaveCount(0);
  const big = (await page.getByTestId("mc-tile-mc-3").boundingBox())!;
  expect(big.width).toBeGreaterThan(1.05 * small.width);
  await expect.poll(async () => (await drawnShare(page, "mc-3")).share).toBeGreaterThan(0.9);
  // only the opened world is on the GPU; the others keep their sockets
  await expect.poll(async () => (await stageNow(page))!.held).toBe(1);
  expect(Object.values(await tilesNow(page)).filter((t) => t.open)).toHaveLength(SLOTS.length);
  await page.keyboard.press("Escape");
  for (const slot of SLOTS) await expect(page.getByTestId(`mc-tile-${slot}`)).toHaveCount(1);
  await expect(page.getByTestId("mc-canvas-mc-3")).toBeFocused();
  // A tile that cannot take the focus back (its server is restoring, so its picture is no
  // button) passes it to the first tile that can: it is never left on the page's body.
  await page.keyboard.press("Enter");
  await expect(page.getByTestId("mc-close")).toBeFocused();
  await page.getByTestId("mc-reset-mc-3").click();
  await expect(page.getByTestId("mc-tile-mc-3")).toHaveAttribute("data-phase", "dark");
  await page.getByTestId("mc-close").focus();
  await page.keyboard.press("Enter");
  for (const slot of SLOTS) await expect(page.getByTestId(`mc-tile-${slot}`)).toHaveCount(1);
  await expect(page.getByTestId("mc-tile-mc-3")).toHaveAttribute("data-phase", "dark");
  await expect(page.getByTestId("mc-canvas-mc-1")).toBeFocused();
  await expect(page.getByTestId("mc-tile-mc-3")).toHaveAttribute("data-phase", "live", { timeout: 15_000 });
  await page.getByTestId("mc-canvas-mc-2").focus();
  await page.keyboard.press("Enter");
  await expect(page.getByTestId("mc-tile-mc-1")).toHaveCount(0);
  await page.getByTestId("mc-close").click();
  for (const slot of SLOTS) await expect(page.getByTestId(`mc-tile-${slot}`)).toHaveCount(1);
});

test("a fork in flight ends when its tiles leave the stage or the copy fails, and the tile shows its own timer when nothing flies", async ({ page }) => {
  await openMinecraft(page);
  await page.getByTestId("mc-fork-mc-3").click();
  // while the picture flies, the tile taking the copy shows the flight, not a second timer
  await expect(page.getByTestId("mc-flight-mc-4")).toBeVisible();
  await expect(page.getByTestId("mc-tile-mc-4")).toHaveClass(/\btaking\b/);
  // opening another server takes both tiles off the stage: no frame is left over it
  await page.getByTestId("mc-canvas-mc-1").click();
  await expect(page.getByTestId("mc-close")).toBeVisible();
  await expect(page.locator(".mc-flight")).toHaveCount(0);
  expect((await stageNow(page))!.flights).toEqual([]);
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("mc-tile-mc-4")).toHaveAttribute("data-phase", "live", { timeout: 15_000 });
  await expect(page.locator(".mc-flight")).toHaveCount(0);
});

test("a lost WebGL context gives the flat map and says the 3D view was lost; a restored one brings the 3D view back", async ({ page }) => {
  await openMinecraft(page);
  expect(await renderer(page)).toBe("3d");
  // lose it mid-fork: the flight's frame goes with the view
  await page.getByTestId("mc-fork-mc-1").click();
  await expect(page.getByTestId("mc-flight-mc-2")).toBeVisible();
  await page.getByTestId("mc-gl").evaluate((c: HTMLCanvasElement) => {
    (window as unknown as { __lose: WEBGL_lose_context }).__lose = c.getContext("webgl2")!.getExtension("WEBGL_lose_context")!;
    (window as unknown as { __lose: WEBGL_lose_context }).__lose.loseContext();
  });
  await expect.poll(() => renderer(page)).toBe("flat");
  await expect(page.locator(".mc-flight")).toHaveCount(0);
  await expect(page.getByTestId("mc-note-mc-1")).toHaveText("3D view lost; showing the flat map.");
  await expect.poll(async () => (await drawnShare(page, "mc-1")).share).toBeGreaterThan(0.9);
  // the tile taking the copy shows its own timer, or the world once it runs: never nothing
  await expect(page.getByTestId("mc-tile-mc-2")).not.toHaveClass(/\btaking\b/);
  await page.evaluate(() => (window as unknown as { __lose: WEBGL_lose_context }).__lose.restoreContext());
  await expect.poll(() => renderer(page), { timeout: 15_000 }).toBe("3d");
  await expect.poll(async () => (await stageNow(page))?.held ?? 0, { timeout: 15_000 }).toBeGreaterThan(0);
  await expect(page.getByText("3D view lost; showing the flat map.")).toHaveCount(0);
  await expect.poll(async () => (await drawnShare(page, "mc-1")).share).toBeGreaterThan(0.9);
});

// ---- a fleet of hundreds: the field ----------------------------------------------------------

test.describe("a fleet of hundreds of servers on one host", () => {
  const fieldNow = (page: Page) => page.evaluate(() => window.__minecraft!.view().field);
  const resize = async (servers: number, perHost: number) => {
    await admin("/mock/minecraft", { servers, per_host: perHost, status_off: false });
    await expect.poll(async () => (await mockMc()).tiles.every((t) => t.phase === "running"), { timeout: 15_000 }).toBe(true);
  };
  const openField = async (page: Page, servers: number) => {
    await page.goto("/?mode=minecraft&debug=1");
    await page.waitForFunction(() => window.__minecraft !== undefined);
    await expect(page.getByTestId("mc-field-servers")).toHaveText(`${servers} servers on one host`, { timeout: 20_000 });
    await expect(page.getByTestId("mc-field-states")).toHaveText(`${servers} running`, { timeout: 20_000 });
  };

  test.beforeAll(() => resize(512, 512));
  test.afterAll(() => resize(4, 8));

  test("512 servers are 512 cells with real state, six of them drawn in 3D, and only those six open a view socket", async ({ page }, info) => {
    await openField(page, 512);
    // the headline: the count, and the density from the host's own readings
    await expect(page.getByTestId("mc-field-density")).toHaveText(
      isPhone(info) ? /^\d+\.\d per busy CPU · \d+\.\d per GiB in use$/ : /^\d+\.\d per busy CPU \(\d+\.\d of 192 busy\)$/,
    );
    if (!isPhone(info)) await expect(page.getByTestId("mc-field-density-mem")).toHaveText(/^\d+\.\d per GiB in use \(\d+ GiB\)$/);
    // the field's words fit their room: no line is cut off or laid over another
    const text = await page.locator(".mc-field-text").evaluate((el) => ({ room: el.clientHeight, need: el.scrollHeight, cut: [...el.querySelectorAll("p")].filter((p) => p.scrollWidth > p.clientWidth + 1).length }));
    expect(text.need).toBeLessThanOrEqual(text.room);
    expect(text.cut).toBe(0);
    await expect(page.getByTestId("mc-up")).toHaveText("512 of 512 servers up");
    const f = (await fieldNow(page))!;
    expect(f.cells).toHaveLength(512);
    expect(f.cells.every((c) => c.phase === "running" && c.tps !== null && c.tps >= 19.9 && c.players === 3 && c.ageS !== null)).toBe(true);
    // six 3D tiles, each a different server, ringed in the field; six sockets and six worlds on the GPU, not 512
    expect(f.shown).toHaveLength(6);
    await expect(page.locator(".mc-tile")).toHaveCount(6);
    await expect.poll(async () => Object.values(await tilesNow(page)).filter((t) => t.open).length, { timeout: 20_000 }).toBe(6);
    expect(Object.keys(await tilesNow(page))).toHaveLength(6);
    await expect.poll(async () => (await stageNow(page))?.held ?? 0, { timeout: 20_000 }).toBe(6);
    // the field and the tiles share the stage without overlapping, inside the viewport, and the page does not scroll sideways
    // (read in one go: the tiles cut to other servers every few seconds)
    const boxes = await page.evaluate(() => {
      const box = (el: Element) => {
        const r = el.getBoundingClientRect();
        return { x: r.x, y: r.y, w: r.width, h: r.height };
      };
      return { field: box(document.querySelector('[data-testid="mc-field"]')!), tiles: [...document.querySelectorAll(".mc-tile")].map(box) };
    });
    const vp = page.viewportSize()!;
    expect(boxes.tiles).toHaveLength(6);
    expect(boxes.field.x).toBeGreaterThanOrEqual(0);
    expect(boxes.field.x + boxes.field.w).toBeLessThanOrEqual(vp.width + 0.5);
    for (const t of boxes.tiles) {
      expect(t.x >= boxes.field.x + boxes.field.w || t.y >= boxes.field.y + boxes.field.h, JSON.stringify(t)).toBe(true);
      expect(t.x + t.w).toBeLessThanOrEqual(vp.width + 0.5);
      expect(t.w).toBeGreaterThan(60);
    }
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
    // on a laptop every cell is on screen at once; on a phone a cell is never smaller than a finger can pick, and the field scrolls when they do not all fit
    const scroller = page.locator(".mc-field-scroll");
    expect(f.layout!.cell).toBeGreaterThanOrEqual(9);
    if (!isPhone(info)) expect(f.layout!.scrolls).toBe(false);
    expect(await scroller.evaluate((el) => el.scrollHeight > el.clientHeight + 1)).toBe(f.layout!.scrolls);
    // the field's own rows fit under the cells: the stepper with what one cell is, and what the marks mean
    const under = await page.evaluate(() => {
      const r = (id: string) => document.querySelector(`[data-testid="${id}"]`)!.getBoundingClientRect();
      const field = r("mc-field");
      return { fieldBottom: field.bottom, legendBottom: r("mc-field-legend").bottom, legendTop: r("mc-field-legend").top, readoutBottom: r("mc-cell-readout").bottom, scrollBottom: document.querySelector(".mc-field-scroll")!.getBoundingClientRect().bottom };
    });
    expect(under.legendBottom).toBeLessThanOrEqual(under.fieldBottom + 0.5);
    expect(under.readoutBottom).toBeLessThanOrEqual(under.legendTop + 0.5);
    expect(under.scrollBottom).toBeLessThanOrEqual(under.legendTop + 0.5);
    await expect(page.getByTestId("mc-field-legend")).toHaveText("20 TPSslowerrestoringforkingfailednot reportedringed: in 3Dlighter: just up");
    // every word of it is inside its room: none cut at the right edge or pushed below
    const legend = await page.getByTestId("mc-field-legend").evaluate((el) => {
      const box = el.getBoundingClientRect();
      return [...el.children].map((c) => c.getBoundingClientRect()).every((r) => r.left >= box.left - 0.5 && r.right <= box.right + 0.5 && r.bottom <= box.bottom + 0.5);
    });
    expect(legend).toBe(true);
    // one canvas for every cell: a paint draws all 512, and only when something changed
    await expect(page.locator(".mc-field canvas")).toHaveCount(1);
    const a = await page.evaluate(() => window.__minecraft!.field());
    expect(a.cells).toBe(512);
    await expect.poll(async () => (await page.evaluate(() => window.__minecraft!.field())).paints).toBeGreaterThan(a.paints);
    const frames = await page.evaluate(
      () =>
        new Promise<{ paints: number; frames: number }>((done) => {
          const from = window.__minecraft!.field().paints;
          let n = 0;
          const tick = () => (++n < 120 ? requestAnimationFrame(tick) : done({ paints: window.__minecraft!.field().paints - from, frames: n }));
          requestAnimationFrame(tick);
        }),
    );
    // 120 animation frames, a handful of paints (the rows come once a second)
    expect(frames.paints).toBeLessThan(30);
    const dir = process.env.FLEET_SCREENSHOT_DIR;
    if (dir) await page.screenshot({ path: `${dir}/minecraft-field-512-${info.project.name}.png` });
  });

  test("a cell says what its server is, and picking it brings that server into a 3D tile until it is unpinned", async ({ page }, info) => {
    await openField(page, 512);
    const f = (await fieldNow(page))!;
    const phone = isPhone(info);
    const says = (n: number) => (phone ? new RegExp(`^mc-${n} · running · \\d+\\.\\d TPS$`) : new RegExp(`^mc-${n} · running · \\d+\\.\\d TPS · 3 players · up .+ · restored in \\d+\\.\\d\\d s$`));
    // a server in the first rows that is not in a tile now
    const i = Array.from({ length: 40 }, (_, k) => k).find((k) => !f.shown.includes(k + 1))!;
    const canvas = (await page.getByTestId("mc-field-canvas").boundingBox())!;
    const at = { x: canvas.x + (i % f.layout!.cols) * f.layout!.cell + f.layout!.cell / 2, y: canvas.y + Math.floor(i / f.layout!.cols) * f.layout!.cell + f.layout!.cell / 2 };
    await expect(page.getByTestId("mc-cell-readout")).toHaveText(phone ? "Tap a server, or step to one." : "Pick a server to see it in 3D.");
    await expect(page.getByTestId("mc-cell-show")).toBeDisabled();
    await page.mouse.move(at.x, at.y);
    await expect(page.getByTestId("mc-cell-readout")).toHaveText(says(i + 1));
    await expect(page.getByTestId(`mc-tile-mc-${i + 1}`)).toHaveCount(0);
    await page.mouse.click(at.x, at.y);
    await expect(page.getByTestId(`mc-tile-mc-${i + 1}`)).toHaveCount(1);
    const picked = (await fieldNow(page))!;
    expect(picked.shown).toContain(i + 1);
    expect(picked.pinned).toEqual([i + 1]);
    await expect(page.locator(".mc-tile")).toHaveCount(6);
    // its own view socket opens and its world is drawn; its tile has what every tile has, and says it is pinned
    await expect.poll(async () => (await drawnShare(page, `mc-${i + 1}`)).share, { timeout: 20_000 }).toBeGreaterThan(0.9);
    await expect(page.getByTestId(`mc-tps-mc-${i + 1}`)).toHaveText(/^\d+\.\d TPS$/);
    await expect(page.getByTestId(`mc-up-in-mc-${i + 1}`)).toHaveText(/^Up in \d+\.\d\d s$/);
    for (const b of ["reset", "fork"]) await expect(page.getByTestId(`mc-${b}-mc-${i + 1}`)).toBeEnabled();
    await expect(page.getByTestId(`mc-unpin-mc-${i + 1}`)).toBeVisible();
    // an action names the server itself: Reset on this tile resets this server and no other
    await page.getByTestId(`mc-reset-mc-${i + 1}`).click();
    await expect.poll(async () => (await record()).intents.filter((x) => x.name === "minecraft_reset").at(-1)?.args).toEqual({ slot: `mc-${i + 1}` });
    await expect.poll(async () => (await fieldNow(page))!.cells.filter((c) => c.phase === "restoring").map((c) => c.n), { timeout: 10_000 }).toEqual([i + 1]);
    // it stays on its tile through its restore, and is given back to the rotation by Unpin
    await expect(page.getByTestId(`mc-tile-mc-${i + 1}`)).toHaveAttribute("data-phase", "live", { timeout: 20_000 });
    await page.getByTestId(`mc-unpin-mc-${i + 1}`).click();
    await expect(page.getByTestId(`mc-tile-mc-${i + 1}`)).toHaveCount(0, { timeout: 15_000 });
    expect((await fieldNow(page))!.pinned).toEqual([]);
    // A finger: a cell is smaller than a fingertip, so a tap only points. The row under the
    // field says what was hit, steps to the next server either way, and Show picks it.
    if (phone) {
      const now = (await fieldNow(page))!;
      const j = Array.from({ length: 40 }, (_, k) => k).find((k) => k !== i && !now.shown.includes(k + 1) && !now.shown.includes(k + 2))!;
      // the page has scrolled to the tiles by now: find the field again
      await page.getByTestId("mc-field-canvas").scrollIntoViewIfNeeded();
      const c2 = (await page.getByTestId("mc-field-canvas").boundingBox())!;
      await page.touchscreen.tap(c2.x + (j % f.layout!.cols) * f.layout!.cell + f.layout!.cell / 2, c2.y + Math.floor(j / f.layout!.cols) * f.layout!.cell + f.layout!.cell / 2);
      await expect(page.getByTestId("mc-cell-readout")).toHaveText(says(j + 1));
      await page.waitForTimeout(400);
      await expect(page.getByTestId(`mc-tile-mc-${j + 1}`)).toHaveCount(0);
      // each control is a finger's size
      for (const id of ["mc-cell-prev", "mc-cell-next", "mc-cell-show"]) {
        const b = (await page.getByTestId(id).boundingBox())!;
        expect(Math.min(b.width, b.height), id).toBeGreaterThanOrEqual(40);
      }
      await page.getByTestId("mc-cell-next").tap();
      await expect(page.getByTestId("mc-cell-readout")).toHaveText(says(j + 2));
      await page.getByTestId("mc-cell-prev").tap();
      await expect(page.getByTestId("mc-cell-readout")).toHaveText(says(j + 1));
      await page.getByTestId("mc-cell-show").tap();
      await expect(page.getByTestId(`mc-tile-mc-${j + 1}`)).toHaveCount(1);
      // the same row takes the pick back
      await page.getByTestId("mc-cell-unpin").tap();
      await expect(page.getByTestId(`mc-tile-mc-${j + 1}`)).toHaveCount(0, { timeout: 15_000 });
    }
    // The keyboard: the field takes focus, arrows move (and stay put at an edge), the cursor's
    // cell is scrolled into view, Enter picks.
    await page.getByTestId("mc-field-canvas").focus();
    await page.keyboard.press("ArrowRight");
    await expect(page.getByTestId("mc-cell-readout")).toHaveText(/^mc-\d+ · running/);
    const start = Number(/^mc-(\d+)/.exec((await page.getByTestId("mc-cell-readout").textContent())!)![1]);
    const rows = Math.floor((512 - start) / f.layout!.cols);
    for (let k = 0; k < rows + 3; k++) await page.keyboard.press("ArrowDown");
    // down from the last row it can reach goes nowhere: the same column, `rows` rows on
    await expect(page.getByTestId("mc-cell-readout")).toHaveText(says(start + rows * f.layout!.cols));
    const seen = await page.evaluate(
      ({ row, cell }) => {
        const el = document.querySelector(".mc-field-scroll")!;
        return { top: el.scrollTop, view: el.clientHeight, y: row * cell, cell };
      },
      { row: Math.floor((start - 1 + rows * f.layout!.cols) / f.layout!.cols), cell: f.layout!.cell },
    );
    expect(seen.y).toBeGreaterThanOrEqual(seen.top);
    expect(seen.y + seen.cell).toBeLessThanOrEqual(seen.top + seen.view + 0.5);
    for (let k = 0; k < rows + 3; k++) await page.keyboard.press("ArrowUp");
    await expect(page.getByTestId("mc-cell-readout")).toHaveText(says(start));
    expect(await page.evaluate(() => document.querySelector(".mc-field-scroll")!.scrollTop)).toBe(0);
    const last = start + rows * f.layout!.cols;
    for (let k = 0; k < rows; k++) await page.keyboard.press("ArrowDown");
    if (!(await fieldNow(page))!.shown.includes(last)) {
      await page.keyboard.press("Enter");
      await expect(page.getByTestId(`mc-tile-mc-${last}`)).toHaveCount(1);
    }
  });

  test("opening a server from a tile holds it until the viewer closes it, and the field is drawn again after", async ({ browser }, info) => {
    // motion reduced: nothing in the field moves by itself, so a field left blank would stay blank
    const context = await browser.newContext({ ...info.project.use, reducedMotion: "reduce" });
    const page = await context.newPage();
    await openField(page, 512);
    const f = (await fieldNow(page))!;
    const n = f.shown[5]!;
    await page.getByTestId(`mc-canvas-mc-${n}`).click();
    await expect(page.getByTestId("mc-close")).toBeVisible();
    await expect(page.getByTestId("mc-field")).toHaveCount(0);
    await expect(page.locator(".mc-tile")).toHaveCount(1);
    await page.getByTestId("mc-close").click();
    await expect(page.getByTestId("mc-field")).toHaveCount(1);
    // every cell is painted on the field's new canvas
    await expect
      .poll(() =>
        page.evaluate(() => {
          const c = document.querySelector<HTMLCanvasElement>('[data-testid="mc-field-canvas"]')!;
          const d = c.getContext("2d")!.getImageData(0, 0, c.width, c.height).data;
          let lit = 0;
          for (let k = 3; k < d.length; k += 4) if (d[k] !== 0) lit++;
          return lit / (d.length / 4);
        }),
      )
      .toBeGreaterThan(0.5);
    await context.close();
  });

  test("an opened server stays open past its tile's turn", async ({ page }) => {
    test.slow();
    await openField(page, 512);
    // the tile whose turn ends first (2 s from the page's start)
    const n = (await fieldNow(page))!.shown[5]!;
    await page.getByTestId(`mc-canvas-mc-${n}`).click();
    await expect(page.getByTestId("mc-close")).toBeVisible();
    // two whole turns later it is still the one view (a tile's turn is 12 s)
    await page.waitForTimeout(25_000);
    await expect(page.getByTestId("mc-close")).toBeVisible();
    await expect(page.getByTestId(`mc-tile-mc-${n}`)).toHaveCount(1);
    await expect(page.locator(".mc-tile")).toHaveCount(1);
    await page.keyboard.press("Escape");
    await expect(page.getByTestId("mc-field")).toHaveCount(1);
    // and it is still on its tile when the wall returns
    await expect(page.getByTestId(`mc-tile-mc-${n}`)).toHaveCount(1);
  });

  test("Reset all comes back as a wave: the cells flash back, counted with the rate, then how long the whole took, the same on a page opened after", async ({ page, browser }, info) => {
    await openField(page, 512);
    await page.getByTestId("mc-reset-all").click();
    await expect(page.getByTestId("mc-wave")).toHaveText(/^Reset all: \d+ of 512 back · \d+\.\d a second · \d+\.\d s$/, { timeout: 10_000 });
    await expect(page.getByTestId("mc-field-states")).toHaveText(/^\d+ running · \d+ restoring$/);
    // The tiles keep their servers through it: over the wave's first 5 s at most three tiles cut
    // (one a turn, 2 s apart), where an index into the running servers moved all six every second.
    const cuts = await page.evaluate(
      () =>
        new Promise<number>((done) => {
          let last = window.__minecraft!.view().field!.shown;
          let n = 0;
          const t0 = performance.now();
          const tick = () => {
            const now = window.__minecraft!.view().field!.shown;
            n += now.filter((x, j) => x !== last[j]).length;
            last = now;
            if (performance.now() - t0 < 5000) setTimeout(tick, 100);
            else done(n);
          };
          tick();
        }),
    );
    expect(cuts).toBeLessThanOrEqual(4);
    // the count rises
    const back = async () => Number(/^Reset all: (\d+) of/.exec((await page.getByTestId("mc-wave").textContent()) ?? "")?.[1] ?? "512");
    const early = await back();
    await expect.poll(back, { timeout: 20_000 }).toBeGreaterThan(early);
    await expect(page.getByTestId("mc-wave")).toHaveText(/^Reset all: 512 servers back in \d+ s \(\d+\.\d a second\)$/, { timeout: 60_000 });
    await expect(page.getByTestId("mc-field-states")).toHaveText("512 running");
    // the time stands still once it is whole
    const said = await page.getByTestId("mc-wave").textContent();
    await page.waitForTimeout(1500);
    await expect(page.getByTestId("mc-wave")).toHaveText(said!);
    // A page opened only now reads the reset's time from the rows, not how long ago it looked:
    // the same to the second (a row's age is in whole seconds, so one second either way).
    const took = (text: string | null) => Number(/back in (\d+) s/.exec(text ?? "")![1]);
    await page.waitForTimeout(4000);
    const context = await browser.newContext({ ...info.project.use });
    const late = await context.newPage();
    await openField(late, 512);
    await expect(late.getByTestId("mc-wave")).toHaveText(/^Reset all: 512 servers back in \d+ s \(\d+\.\d a second\)$/);
    expect(Math.abs(took(await late.getByTestId("mc-wave").textContent()) - took(said))).toBeLessThanOrEqual(1);
    await context.close();
  });

  test("servers whose host stops reporting are shown as not reported, never as zero or as running", async ({ page }) => {
    await openField(page, 512);
    await admin("/mock/minecraft", { status_off: true });
    try {
      // no count is given for servers nobody reported: never "0 of 512 servers up"
      await expect(page.getByTestId("mc-field-states")).toHaveText(/^no report for \d+ s$/, { timeout: 15_000 });
      await expect(page.getByTestId("mc-up")).toHaveText(/^512 servers, no report for \d+ s$/);
      await expect(page.getByTestId("mc-field-density")).toHaveCount(0);
      await expect(page.getByTestId("mc-field-density-mem")).toHaveCount(0);
      expect((await fieldNow(page))!.cells.every((c) => c.phase === "none" && c.tps === null)).toBe(true);
    } finally {
      await admin("/mock/minecraft", { status_off: false });
    }
    await expect(page.getByTestId("mc-field-states")).toHaveText("512 running", { timeout: 15_000 });
  });

  test("256 servers fit the same way, in larger cells", async ({ page }, info) => {
    await resize(256, 256);
    try {
      await openField(page, 256);
      const f = (await fieldNow(page))!;
      expect(f.cells).toHaveLength(256);
      await expect(page.locator(".mc-tile")).toHaveCount(6);
      if (!isPhone(info)) expect(f.layout!.scrolls).toBe(false);
      const dir = process.env.FLEET_SCREENSHOT_DIR;
      if (dir) await page.screenshot({ path: `${dir}/minecraft-field-256-${info.project.name}.png` });
    } finally {
      await resize(512, 512);
    }
  });
});
