// The line under each page's title and what its "What is this?" control adds. One source for
// the five pages (wall, phones, potato, windows, minecraft); the views carry the result and
// one component draws it.

export type LandingPage = "wall" | "phones" | "potato" | "windows" | "minecraft";

export const SHARED_LINE = "fcvm starts virtual machines from saved snapshots instead of booting them.";

const WALL_LINE = "Each square is a virtual machine with its own NVIDIA GPU. About half are restored by fcvm; the rest are EC2 GPUs started normally.";
/** The Wall's line while the view lists a Lambda tile: the AWS half is no longer EC2 alone. */
const WALL_LINE_LAMBDA = "Each square is a virtual machine with its own NVIDIA GPU. About half are restored by fcvm; the rest are started on AWS the usual way.";

const LINES: Record<Exclude<LandingPage, "wall">, string> = {
  phones: "Each phone is an Android emulator in its own virtual machine, drawing on its own NVIDIA GPU.",
  potato: "Each player is a virtual machine. After it throws, it is saved as a snapshot and stopped, and must be restored before the ball comes round.",
  windows: "Each tile is a Windows desktop in its own virtual machine, restored from one snapshot.",
  minecraft: "Each tile is a Minecraft server in its own virtual machine, restored from one snapshot with its world loaded.",
};

/** The panel's sentences after the shared line. Only what the line and the labelled controls on the page do not say. */
const ADDED: Record<LandingPage, string[]> = {
  wall: ["After a restart, compare how long each side takes to come back."],
  phones: ["Together they draw one scene. Open a phone to see how long its restore took."],
  potato: ["The hold gets a tenth shorter each lap, until a player is still waking when the ball arrives."],
  windows: ["Taking a tile over allows a fixed set of actions."],
  minecraft: ["Fork copies a running server's world into the next server on its host."],
};

/** What a legend entry draws: a tile in a phase (with the ball when it holds it), or one of the Hold card's marks. */
export type LegendMark =
  | { kind: "tile"; phase: "awake" | "sleeping" | "asleep" | "waking"; wake?: number; holder?: boolean }
  | { kind: "hold-bar" }
  | { kind: "hold-tick" };

export interface LegendEntry {
  key: string;
  mark: LegendMark;
  /** One or two words, drawn under the mark; null for a mark the picture already says. */
  label: string | null;
  /** Spoken in place of a missing label. */
  sr?: string;
  /** A second, smaller line, only where a visitor cannot see the meaning. */
  detail?: string;
}

/** The Potato legend: a strip cut from the ring, in the order the ball meets the players. */
export const POTATO_LEGEND_TITLE = "Legend";
export const POTATO_STRIP: readonly LegendEntry[] = [
  { key: "asleep-done", mark: { kind: "tile", phase: "asleep" }, label: "asleep", detail: "a snapshot on disk; no CPU or memory" },
  { key: "sleeping", mark: { kind: "tile", phase: "sleeping" }, label: "going to sleep" },
  { key: "holder", mark: { kind: "tile", phase: "awake", holder: true }, label: null, sr: "has the ball" },
  { key: "awake", mark: { kind: "tile", phase: "awake" }, label: "awake" },
  { key: "waking", mark: { kind: "tile", phase: "waking", wake: 0.6 }, label: "waking" },
  { key: "asleep-next", mark: { kind: "tile", phase: "asleep" }, label: "asleep" },
];
export const POTATO_CATCH_NOTE = "Colour steps on each catch: proof its memory survived.";
/** The Hold card's two marks, as a small diagram of its meter. */
export const POTATO_HOLD: readonly LegendEntry[] = [
  { key: "hold-bar", mark: { kind: "hold-bar" }, label: "time to sleep and wake" },
  { key: "hold-tick", mark: { kind: "hold-tick" }, label: "slowest player" },
];

export interface LandingView {
  page: LandingPage;
  /** Under the title, always visible. */
  line: string;
  /** The "What is this?" control's label. */
  button: string;
  shared: string;
  /** Sentences the panel adds after the page's line. */
  added: string[];
  legend: { title: string; strip: readonly LegendEntry[]; catchNote: string; hold: readonly LegendEntry[] } | null;
}

export function landingView(page: LandingPage, opts: { lambda?: boolean } = {}): LandingView {
  const line = page === "wall" ? (opts.lambda === true ? WALL_LINE_LAMBDA : WALL_LINE) : LINES[page];
  return {
    page,
    line,
    button: "What is this?",
    shared: SHARED_LINE,
    added: ADDED[page],
    legend: page === "potato" ? { title: POTATO_LEGEND_TITLE, strip: POTATO_STRIP, catchNote: POTATO_CATCH_NOTE, hold: POTATO_HOLD } : null,
  };
}

/** The panel's paragraphs in order. The page's line is added only when the page clamps it. */
export function landingPanelText(v: LandingView, lineClamped: boolean): string[] {
  return [v.shared, ...(lineClamped ? [v.line] : []), ...v.added];
}
