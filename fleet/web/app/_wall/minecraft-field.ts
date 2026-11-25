// Minecraft mode's field: every server a cell on one canvas (no element per cell), painted from
// the view model's cells. A cell flashes when its server comes back from a restore and stays
// lighter for its first minute; the servers in the 3D tiles are ringed. Painted again only when
// something in it changed or a flash runs.

import { cellBox, cellColor, FLASH_MS, youngLift, type FieldCell, type FieldLayout } from "../../core/index.ts";

export interface FieldPaint {
  layout: FieldLayout;
  cells: readonly FieldCell[];
  /** The servers (by number) in the 3D tiles, and those of them a viewer's pick holds there. */
  shown: readonly number[];
  pinned: readonly number[];
  /** The cell under the pointer or the keyboard's cursor (its index), if any. */
  cursor: number | null;
  dpr: number;
  reducedMotion: boolean;
}

export class McField {
  /** When each server (by number) last came back: it flashes for FLASH_MS. */
  private flashes = new Map<number, number>();
  private before = new Map<number, string>();
  private sig = "";
  /** The canvas last painted: another one (the field came back to the page) is blank until painted. */
  private canvas: HTMLCanvasElement | null = null;
  /** Paints so far, and how many cells the last one drew, for the test hook. */
  paints = 0;
  lastCells = 0;

  /** Note which servers just came back (restoring or forking, then running), so they flash. */
  private track(cells: readonly FieldCell[], now: number): void {
    for (const c of cells) {
      const was = this.before.get(c.n);
      if (c.phase === "running" && (was === "restoring" || was === "forking")) this.flashes.set(c.n, now);
      this.before.set(c.n, c.phase);
    }
    for (const [n, at] of this.flashes) if (now - at > FLASH_MS) this.flashes.delete(n);
  }

  /** Paint when the field changed or a flash is running; returns whether it painted. */
  paint(canvas: HTMLCanvasElement, p: FieldPaint, now: number): boolean {
    this.track(p.cells, now);
    const flashing = !p.reducedMotion && this.flashes.size > 0;
    // a young server's lift changes with its age: every 3 s is fine enough to see it fade
    const sig = `${p.layout.cell},${p.layout.cols},${p.dpr},${p.cursor}|${p.shown.join()}|${p.pinned.join()}|${p.cells.map((c) => `${c.phase[0]}${c.tps ?? ""}${youngLift(c) > 0 ? `y${Math.floor(c.ageS! / 3)}` : ""}`).join()}`;
    // A canvas this painter has not painted holds nothing: the same field is painted again on
    // it, with motion or without.
    const fresh = canvas !== this.canvas;
    if (sig === this.sig && !flashing && !fresh) return false;
    this.sig = sig;
    this.canvas = canvas;
    const W = Math.max(1, Math.round(p.layout.w * p.dpr));
    const H = Math.max(1, Math.round(p.layout.h * p.dpr));
    if (canvas.width !== W || canvas.height !== H) {
      canvas.width = W;
      canvas.height = H;
    }
    const ctx = canvas.getContext("2d");
    if (!ctx) return false;
    ctx.setTransform(p.dpr, 0, 0, p.dpr, 0, 0);
    ctx.clearRect(0, 0, p.layout.w, p.layout.h);
    const shown = new Set(p.shown);
    const pinned = new Set(p.pinned);
    p.cells.forEach((c, i) => {
      const b = cellBox(p.layout, i);
      const [r, g, bl] = cellColor(c);
      if (c.phase === "none") {
        // not reported: an outline, never a filled cell that could pass for a state
        ctx.strokeStyle = `rgb(${r},${g},${bl})`;
        ctx.lineWidth = 1;
        ctx.strokeRect(b.x + 0.5, b.y + 0.5, b.w - 1, b.h - 1);
      } else {
        const at = flashing ? this.flashes.get(c.n) : undefined;
        const k = Math.max(at === undefined ? 0 : Math.max(0, 1 - (now - at) / FLASH_MS), youngLift(c));
        ctx.fillStyle = `rgb(${Math.round(r + (255 - r) * k)},${Math.round(g + (255 - g) * k)},${Math.round(bl + (255 - bl) * k)})`;
        ctx.fillRect(b.x, b.y, b.w, b.h);
      }
      if (shown.has(c.n) || i === p.cursor) {
        // in a 3D tile: ringed white, or amber when a viewer's pick holds it there; the cursor's cell blue
        ctx.strokeStyle = i === p.cursor ? "#7cc4ff" : pinned.has(c.n) ? "#ffd27c" : "#ffffff";
        ctx.lineWidth = 2;
        ctx.strokeRect(b.x + 1, b.y + 1, b.w - 2, b.h - 2);
      }
    });
    this.paints++;
    this.lastCells = p.cells.length;
    return true;
  }
}
