import { describe, expect, it } from "vitest";
import { landingPanelText, landingView, POTATO_STRIP, SHARED_LINE, type LandingPage } from "../landing.ts";

const LINES: Record<LandingPage, string> = {
  wall: "Each square is a virtual machine with its own NVIDIA GPU. About half are restored by fcvm; the rest are EC2 GPUs started normally.",
  phones: "Each phone is an Android emulator in its own virtual machine, drawing on its own NVIDIA GPU.",
  potato: "Each player is a virtual machine. After it throws, it is saved as a snapshot and stopped, and must be restored before the ball comes round.",
  windows: "Each tile is a Windows desktop in its own virtual machine, restored from one snapshot.",
  minecraft: "Each tile is a Minecraft server in its own virtual machine, restored from one snapshot with its world loaded.",
};
const PANELS: Record<LandingPage, string[]> = {
  wall: ["Press Restart all and compare how long each side takes to come back."],
  phones: ["Together they draw one scene. Open a phone to see how long its restore took, or take it over for a short time."],
  potato: ["The hold gets a tenth shorter each lap, until a player is still waking when the ball arrives."],
  windows: ["Click a tile to reset it, or take one over for a fixed set of actions."],
  minecraft: ["Fork copies a running server's world into the next server on its host. A Java Edition client can join at the address shown."],
};

describe("landing text", () => {
  for (const page of Object.keys(LINES) as LandingPage[]) {
    it(`${page}: the line and the panel are the agreed words`, () => {
      const v = landingView(page);
      expect(v.line).toBe(LINES[page]);
      expect(v.shared).toBe(SHARED_LINE);
      expect(landingPanelText(v)).toEqual([SHARED_LINE, LINES[page], ...PANELS[page]]);
    });
  }
  it("the Wall says EC2 only without Lambda", () => {
    expect(landingView("wall").line).toContain("EC2");
    expect(landingView("wall", { lambda: true }).line).not.toContain("EC2");
    expect(landingView("wall", { lambda: true }).line).toContain("started on AWS the usual way");
  });
  it("only potato has a legend: the strip in the ball's order, and the Hold card's two marks", () => {
    for (const p of ["wall", "phones", "windows", "minecraft"] as const) expect(landingView(p).legend).toBeNull();
    const l = landingView("potato").legend!;
    expect(l.strip).toBe(POTATO_STRIP);
    expect(l.strip.map((e) => e.label ?? e.sr)).toEqual(["asleep", "going to sleep", "has the ball", "awake", "waking", "asleep"]);
    expect(l.strip.filter((e) => e.detail).map((e) => e.detail)).toEqual(["saved as a snapshot; uses no CPU"]);
    expect(l.catchNote).toBe("Colour steps on each catch: proof its memory survived.");
  });
});
