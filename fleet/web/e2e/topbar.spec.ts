// The top bar of every mode (the title row, the mode switch and the controls beside it) on
// phone and desktop screens (#2099): each control lies wholly inside the viewport, the page
// does not scroll sideways, the mode switch hides no link behind its own scroll, and on a
// touch screen each control is at least 44 px tall. The modes are read from the switch the
// wall page draws, so a mode added to it is covered without a change here.
import { expect, test, type Page } from "@playwright/test";
import { mkdirSync } from "node:fs";

const SIZES = [
  { name: "phone-390", width: 390, height: 844, touch: true },
  { name: "phone-360", width: 360, height: 780, touch: true },
  { name: "desktop-1280", width: 1280, height: 800, touch: false },
  { name: "desktop-1728", width: 1728, height: 996, touch: false },
];

const TOP_CONTROLS = "header a, header button, header [role=button], .hud-top a, .hud-top button, .hud-top [role=button], nav.mode-switch a, nav.mode-switch button";

async function modes(page: Page): Promise<string[]> {
  await page.goto("/");
  const hrefs = await page.locator("nav.mode-switch a.mode-link").evaluateAll((as) => as.map((a) => (a as HTMLAnchorElement).getAttribute("href") ?? ""));
  expect(hrefs.length).toBeGreaterThanOrEqual(5);
  return hrefs;
}

for (const size of SIZES) {
  test.describe(size.name, () => {
    test.use({ viewport: { width: size.width, height: size.height }, hasTouch: size.touch, isMobile: size.touch });

    test("every top-bar control of every mode is inside the viewport", async ({ page }) => {
      const hrefs = await modes(page);
      const shots = process.env.FLEET_TOPBAR_SHOTS;
      if (shots) mkdirSync(shots, { recursive: true });
      const faults: string[] = [];
      for (const href of hrefs) {
        await page.goto(href);
        await expect(page.locator("nav.mode-switch")).toBeVisible();
        // The controls that arrive with the fleet's first snapshot are awaited, so the
        // measurement below always includes them.
        if (href === "/") await expect(page.getByTestId("stream-mode")).toBeVisible();
        if (href.includes("phones")) await expect(page.getByTestId("phones-zoom")).toBeVisible();
        if (href.includes("minecraft")) await expect(page.getByTestId("mc-join-copy")).toBeVisible();
        if (shots) await page.screenshot({ path: `${shots}/${size.name}-${href.replace(/\W+/g, "_")}.png` });
        const report = await page.evaluate(
          ([sel, coarse]) => {
            const out: string[] = [];
            const w = window.innerWidth;
            const seen = new Set<Element>();
            for (const el of Array.from(document.querySelectorAll(sel as string))) {
              if (seen.has(el)) continue;
              seen.add(el);
              const r = el.getBoundingClientRect();
              const cs = getComputedStyle(el);
              if (r.width === 0 || r.height === 0 || cs.visibility === "hidden" || cs.display === "none") continue;
              const name = `${el.tagName.toLowerCase()}[${el.getAttribute("data-testid") ?? el.textContent?.trim().slice(0, 20)}]`;
              if (r.left < -0.5 || r.right > w + 0.5) out.push(`${name} spans x ${Math.round(r.left)}..${Math.round(r.right)} of ${w}`);
              if (coarse && r.height < 43.5) out.push(`${name} is ${Math.round(r.height)} px tall`);
            }
            const nav = document.querySelector("nav.mode-switch");
            if (nav && nav.scrollWidth > nav.clientWidth + 1) out.push(`mode switch scrolls inside itself (${nav.scrollWidth} > ${nav.clientWidth})`);
            if (document.documentElement.scrollWidth > w) out.push(`page scrolls sideways (${document.documentElement.scrollWidth} > ${w})`);
            return out;
          },
          [TOP_CONTROLS, size.touch] as const,
        );
        for (const f of report) faults.push(`${href}: ${f}`);
      }
      expect(faults, `${size.name}\n${faults.join("\n")}`).toEqual([]);
    });
  });
}
