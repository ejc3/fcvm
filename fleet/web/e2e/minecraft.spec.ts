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

/** How much of a tile's canvas is not the page's background (#06080b), and how many colours it holds. */
async function drawnShare(page: Page, slot: string): Promise<{ share: number; colours: number }> {
  return page.getByTestId(`mc-canvas-${slot}`).evaluate((c: HTMLCanvasElement) => {
    const ctx = c.getContext("2d")!;
    const d = ctx.getImageData(0, 0, c.width, c.height).data;
    let drawn = 0;
    const colours = new Set<number>();
    for (let i = 0; i < d.length; i += 4) {
      if (Math.abs(d[i]! - 6) + Math.abs(d[i + 1]! - 8) + Math.abs(d[i + 2]! - 11) > 12) drawn++;
      if (i % 64 === 0) colours.add((d[i]! << 16) | (d[i + 1]! << 8) | d[i + 2]!);
    }
    return { share: drawn / (d.length / 4), colours: colours.size };
  });
}

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
  await expect(page.getByTestId("mc-last-fork")).toHaveText("Fork: not used yet");
  await expect(page.getByTestId("mc-join")).toHaveText("Join from Minecraft: Java Edition 26.3 at mc-1.mc.example.test, port 25565. Every tile has its own address, under it.");
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
  await expect(page.getByTestId("mc-last-fork")).toHaveText(/^Last fork: mc-3 to mc-4 in \d+\.\d\d s \(snapshot 0\.60 s, restore \d+\.\d\d s\)$/);
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
  const before = await drawnShare(page, "mc-1");
  await act("KeyE", "place");
  await expect(page.getByTestId("mc-first-change")).toHaveText(/^Key to first change: last \d+ ms, median \d+ ms \(1 key\)$/, { timeout: 5000 });
  expect((await drawnShare(page, "mc-1")).share).toBeGreaterThan(0.9);
  expect(before.share).toBeGreaterThan(0.9);
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
