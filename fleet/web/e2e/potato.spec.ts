// Potato mode (`?mode=potato`) against the mock fleet with eight potato players
// (playwright.potato.config.ts): the ring and the ball, Faster and Restart, a drop and the
// ladder, and the page on a phone and in light and dark.

import { expect, test, type Page } from "@playwright/test";
import { potatoColour } from "../core/potato.ts";
import { admin, mockState } from "./helpers.ts";

type MockRound = { id: number; size: number; started_at: number; hold_ms: number; pace_ms: number | null; holder: number | null };
type MockPotato = { size: number; round: MockRound | null; next_round_at: number | null; ladder: { size: number; rounds: number; record_hold_ms: number | null }[] };
const mockPotato = async () => ((await mockState()).state as unknown as { potato: MockPotato }).potato;

const view = (page: Page) => page.evaluate(() => window.__potato!.view());
const tiles = (page: Page) => page.getByTestId("potato-tile");

/** Start the game over at a size, its first round half a second from now. */
async function startAt(size: number): Promise<void> {
  await admin("/mock/potato", { size, next_in_ms: 500 });
  await expect.poll(async () => (await mockPotato()).round?.size ?? null, { timeout: 10_000 }).toBe(size);
}

async function openPotato(page: Page): Promise<void> {
  await page.goto("/?mode=potato&debug=1");
  await page.waitForFunction(() => window.__potato !== undefined);
}

/** Every visible element with text of its own under 14 px, or under 4.5:1 against what is behind it (opacity included). */
function textProblems(page: Page): Promise<string[]> {
  return page.evaluate(() => {
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
      if (!own || el.closest(".sr-only")) continue;
      const r = el.getBoundingClientRect();
      const cs = getComputedStyle(el);
      if (r.width === 0 || r.height === 0 || cs.visibility === "hidden" || cs.display === "none") continue;
      const size = parseFloat(cs.fontSize);
      // Opacity on the element or an ancestor fades the text into what is behind it.
      let opacity = 1;
      for (let e: Element | null = el; e; e = e.parentElement) opacity *= Number(getComputedStyle(e).opacity);
      const bgc = behind(el);
      const fg = parse(cs.color).slice(0, 3).map((v, i) => v * opacity + bgc[i]! * (1 - opacity));
      const a = lum(fg) + 0.05;
      const b = lum(bgc) + 0.05;
      const ratio = Math.max(a, b) / Math.min(a, b);
      if (size < 14 || ratio < 4.5) out.push(`${el.tagName}.${el.className} "${el.textContent!.trim().slice(0, 30)}" ${size}px ${ratio.toFixed(2)}:1`);
    }
    return out;
  });
}

test.describe.configure({ mode: "serial" });

test("draws the ring: one tile per player on a circle, each in its colour, and no coordinator", async ({ page }) => {
  await startAt(8);
  await openPotato(page);
  await expect(tiles(page)).toHaveCount(8);
  await expect(page.getByTestId("potato-caption")).toContainText("Ring of 8");
  const centres = await tiles(page).evaluateAll((els) =>
    els.map((e) => {
      const r = e.getBoundingClientRect();
      return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
    }),
  );
  const stage = (await page.getByTestId("potato-stage").boundingBox())!;
  const c = { x: stage.x + stage.width / 2, y: stage.y + stage.height / 2 };
  const radii = centres.map((p) => Math.hypot(p.x - c.x, p.y - c.y));
  for (const r of radii) expect(r).toBeCloseTo(radii[0]!, 0);
  expect(radii[0]).toBeGreaterThan(50);
  // player 0 at the top, the rest clockwise
  expect(centres[0]!.y).toBeLessThan(c.y);
  expect(centres[2]!.x).toBeGreaterThan(c.x);
  expect(await page.getByText(/coordinator/i).count()).toBe(0);
  // Each tile carries its own player's colour from the palette, dimmed tiles included. The
  // computed fill is not compared: a dimmed tile's is a color-mix() (`color(srgb ...)`), and one
  // changing phase is mid-transition (`oklab(...)`).
  await expect(page.locator('[data-testid="potato-tile"][data-phase="asleep"]').first()).toBeAttached({ timeout: 10_000 });
  const mismatches = async () => {
    const seen = await page.evaluate(() => {
      const p = window.__potato!.client.getState().accepted!.potato!;
      return {
        colours: p.colours,
        players: p.players.map((x) => [x.player, x.colour] as const),
        tiles: [...document.querySelectorAll<HTMLElement>('[data-testid="potato-tile"]')].map((e) => [Number(e.dataset["player"]), e.style.getPropertyValue("--c")] as const),
      };
    });
    const index = new Map(seen.players);
    return seen.tiles.filter(([n, c]) => !index.has(n) || c !== potatoColour(index.get(n)!, seen.colours)).map(([n, c]) => `player ${n}: ${c}`);
  };
  // The DOM follows the snapshot on the next frame, so a catch can land between the two reads.
  await expect.poll(mismatches, { timeout: 5000 }).toEqual([]);
});

test("the ball moves from tile to tile, and players sleep and wake", async ({ page }) => {
  await startAt(8);
  await openPotato(page);
  const ball = page.getByTestId("potato-ball");
  await expect(ball).toBeVisible();
  const spots = new Set<string>();
  const holders = new Set<string>();
  const phases = new Set<string>();
  for (let i = 0; i < 30; i++) {
    const b = (await ball.boundingBox())!;
    spots.add(`${Math.round(b.x / 4)},${Math.round(b.y / 4)}`);
    const h = await page.locator('[data-testid="potato-tile"][data-holder]').getAttribute("data-player").catch(() => null);
    if (h !== null) holders.add(h);
    for (const p of await tiles(page).evaluateAll((els) => els.map((e) => e.getAttribute("data-phase")))) phases.add(p ?? "");
    await page.waitForTimeout(100);
  }
  expect(spots.size).toBeGreaterThanOrEqual(5);
  expect(holders.size).toBeGreaterThanOrEqual(3);
  expect(phases.has("asleep")).toBe(true);
  expect(phases.has("awake")).toBe(true);
  // a waking tile has its bar, and the line under it its milliseconds
  await expect(page.locator('[data-testid="potato-tile"][data-phase="waking"] .wake-bar').first()).toBeAttached({ timeout: 10_000 });
  await expect
    .poll(async () => (await view(page)).ring!.tiles.find((t) => t.phase === "waking")?.line ?? "", { timeout: 10_000 })
    .toMatch(/^waking \d+ ms$/);
});

test("Faster caps the hold a fifth lower, then waits 2 s", async ({ page }) => {
  await startAt(8);
  await openPotato(page);
  await expect(page.getByTestId("potato-hold")).toHaveText(/^\d+ ms$/);
  const faster = page.getByTestId("potato-faster");
  await expect(faster).toBeEnabled();
  await expect(page.getByTestId("potato-faster-note")).toHaveText(/^cuts the hold to \d+ ms$/);
  const before = (await mockPotato()).round!.hold_ms;
  await faster.click();
  await expect.poll(async () => (await mockPotato()).round?.pace_ms ?? null).not.toBeNull();
  const pace = (await mockPotato()).round!.pace_ms!;
  expect(pace).toBeLessThanOrEqual(before - Math.floor(before / 5));
  await expect(page.getByTestId("potato-pace-cap")).toHaveText(`Faster: holds capped at ${pace} ms`);
  const shown = Number((await page.getByTestId("potato-hold").textContent())!.replace(" ms", ""));
  expect(shown).toBeLessThanOrEqual(pace);
  await expect(faster).toBeDisabled();
  await expect(page.getByTestId("potato-faster-note")).toHaveText(/^again in [12] s$/);
  await expect(faster).toBeEnabled({ timeout: 5000 });
});

test("Restart waits 10 s into a round, then starts the same size again without a record", async ({ page }) => {
  await startAt(8);
  await openPotato(page);
  const restart = page.getByTestId("potato-restart");
  await expect(restart).toBeDisabled();
  await expect(page.getByTestId("potato-restart-note")).toHaveText(/^possible 10 s into a round, in \d+ s$/);
  const id = (await mockPotato()).round!.id;
  await expect(restart).toBeEnabled({ timeout: 15_000 });
  await expect(page.getByTestId("potato-restart-note")).toHaveText("starts the ring of 8 again");
  await restart.click();
  await expect.poll(async () => (await mockPotato()).round?.id ?? null, { timeout: 10_000 }).toBe(id + 1);
  const p = await mockPotato();
  expect(p.round!.size).toBe(8);
  expect(p.ladder.find((g) => g.size === 8)!.rounds).toBe(0);
  await expect(restart).toBeDisabled();
  await expect(page.getByTestId("potato-restart-note")).toHaveText(/^again in \d+ s$/);
});

test("a drop: the card names the player and the late stage, the ladder fills in, and the next size starts", async ({ page }) => {
  await startAt(2);
  await openPotato(page);
  await expect(tiles(page)).toHaveCount(2);
  const card = page.getByTestId("potato-drop");
  await expect(card).toBeVisible({ timeout: 30_000 });
  await expect(page.getByTestId("potato-drop-headline")).toHaveText(/^Player [01] was still (asleep|going to sleep|waking)(,| when)/);
  await expect(page.getByTestId("potato-drop-next")).toHaveText(/^Next: a ring of 4 in \d s$/);
  await expect(page.getByTestId("potato-ball")).toHaveAttribute("data-dropped", "true");
  const row2 = page.getByTestId("potato-rung-2");
  await expect(row2.getByTestId("potato-rung-record")).toHaveText(/^(\d+ ms|no lap completed)$/);
  await expect(row2.getByTestId("potato-rung-broke")).toHaveText(/^player [01] still (asleep|going to sleep|waking) \(\d+ ms\), lap \d+ at \d+ ms$/);
  await expect(page.getByTestId("potato-rung-4")).toHaveAttribute("aria-current", "true");
  await expect(page.getByTestId("potato-record-longest-round")).toContainText(/\d+ catch(es)? a ring of 2/);
  // The ring of 4 starts after the 3 s pause, and the card goes.
  await expect(tiles(page)).toHaveCount(4, { timeout: 10_000 });
  await expect(card).toHaveCount(0);
});

test("on a phone: no sideways scroll, and no text under 14 px, during a round and after a drop", async ({ browser }) => {
  await startAt(2);
  const ctx = await browser.newContext({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true, deviceScaleFactor: 1 });
  const page = await ctx.newPage();
  await openPotato(page);
  await expect(tiles(page)).toHaveCount(2);
  const sideways = () =>
    page.evaluate(() => {
      const main = document.querySelector('[data-testid="potato"]')!;
      return Math.max(document.documentElement.scrollWidth - window.innerWidth, main.scrollWidth - main.clientWidth);
    });
  expect(await sideways()).toBeLessThanOrEqual(0);
  expect(await textProblems(page)).toEqual([]);
  // the whole ring is on screen
  for (const b of await tiles(page).evaluateAll((els) => els.map((e) => e.getBoundingClientRect().toJSON() as DOMRect))) {
    expect(b.left).toBeGreaterThanOrEqual(0);
    expect(b.right).toBeLessThanOrEqual(390);
  }
  await expect(page.getByTestId("potato-drop")).toBeVisible({ timeout: 30_000 });
  expect(await sideways()).toBeLessThanOrEqual(0);
  expect(await textProblems(page)).toEqual([]);
  await ctx.close();
});

for (const scheme of ["light", "dark"] as const) {
  test(`${scheme}: the page follows the viewer's theme and its text reads at 4.5:1 or better`, async ({ page }) => {
    await startAt(8);
    await page.emulateMedia({ colorScheme: scheme });
    await openPotato(page);
    await expect(tiles(page)).toHaveCount(8);
    await expect(page.getByTestId("potato-pace")).toBeVisible();
    // with a player asleep, so its dimmed tile's number is checked too
    await expect(page.locator('[data-testid="potato-tile"][data-phase="asleep"]').first()).toBeAttached({ timeout: 10_000 });
    const bg = await page.getByTestId("potato").evaluate((e) => getComputedStyle(e).backgroundColor);
    const [r, g, b] = bg.match(/\d+/g)!.map(Number) as [number, number, number];
    if (scheme === "light") expect(Math.min(r, g, b)).toBeGreaterThan(200);
    else expect(Math.max(r, g, b)).toBeLessThan(40);
    expect(await textProblems(page)).toEqual([]);
  });
}

test("the header switch: the wall links to the potato page, which links back to the wall and the phones", async ({ page }) => {
  await page.goto("/?debug=1");
  await page.waitForFunction(() => window.__fleet !== undefined);
  await page.getByTestId("mode-potato").click();
  await page.waitForFunction(() => window.__potato !== undefined);
  await expect(page).toHaveURL(/\/\?debug=1&mode=potato$/);
  await expect(page.getByTestId("mode-potato")).toHaveAttribute("aria-current", "page");
  await expect(page.getByTestId("mode-wall")).toHaveAttribute("href", "/?debug=1");
  await expect(page.getByTestId("mode-phones")).toHaveAttribute("href", "/?debug=1&mode=phones");
});
