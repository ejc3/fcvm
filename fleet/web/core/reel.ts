// The recording shown while the fleet is asleep or waking: a reel of the real
// wall, with a badge that says when it was recorded and what it shows. Every
// date and number on it comes from /media/wall-reel.json.

import * as fmt from "./format.ts";

/** What /media/wall-reel.json says about the recording (assumed shape). */
export interface ReelMeta {
  /** When it was recorded, Unix ms. */
  recorded_at: number;
  duration_s: number;
  /** The numbers measured in the recording, or null when it shows no race. */
  measured: { gpus: number; fcvm_ms: number; ec2_ms: number } | null;
}

/** Read wall-reel.json; null for anything that is not that shape (the badge then stays off). */
export function parseReelMeta(v: unknown): ReelMeta | null {
  if (typeof v !== "object" || v === null || Array.isArray(v)) return null;
  const o = v as Record<string, unknown>;
  const n = (x: unknown) => (typeof x === "number" && Number.isFinite(x) ? x : null);
  const at = n(o["recorded_at"]);
  const dur = n(o["duration_s"]);
  if (at === null || dur === null) return null;
  const m = o["measured"];
  let measured: ReelMeta["measured"] = null;
  if (typeof m === "object" && m !== null && !Array.isArray(m)) {
    const mm = m as Record<string, unknown>;
    const g = n(mm["gpus"]);
    const f = n(mm["fcvm_ms"]);
    const e = n(mm["ec2_ms"]);
    if (g !== null && f !== null && e !== null) measured = { gpus: g, fcvm_ms: f, ec2_ms: e };
  }
  return { recorded_at: at, duration_s: dur, measured };
}

export interface ReelSource {
  src: string;
  type: string;
}

export interface ReelView {
  /** On screen: the fleet is asleep or waking, or the crossfade to the live image is not done. */
  show: boolean;
  /** The live image is whole: fade the recording out. */
  leaving: boolean;
  /** Start playing at once; otherwise show the poster and a button. */
  autoplay: boolean;
  sources: ReelSource[];
  track: { src: string; label: string; lang: string };
  /** "Recorded 22 Oct 2026, 23:14 UTC. The live wall is waking up.", or "" without the metadata. */
  badge: string;
  /** What the recording shows, from its measured numbers, or "". */
  caption: string;
  play: string;
  /** Why it does not play by itself, next to the button. */
  manualNote: string;
}

export interface ReelInput {
  /** Where /media is served from: "/media" on the page's origin, or a full URL. */
  base: string;
  meta: ReelMeta | null;
  /** asleep, waking, or awake. */
  power: string;
  reelOn: boolean;
  allLive: boolean;
  reducedMotion: boolean;
  saveData: boolean;
}

export function reelView(i: ReelInput): ReelView {
  const asleep = i.power === "asleep";
  const sleeping = i.power !== "awake";
  const base = i.base.replace(/\/$/, "");
  const m = i.meta;
  const tail = asleep ? "The live wall is asleep; it wakes when someone watches." : "The live wall is waking up.";
  return {
    show: sleeping || i.reelOn,
    leaving: !sleeping && i.allLive,
    autoplay: !i.reducedMotion && !i.saveData,
    sources: [
      { src: `${base}/wall-reel.mp4`, type: "video/mp4" },
      { src: `${base}/wall-reel.webm`, type: "video/webm" },
    ],
    track: { src: `${base}/wall-reel.vtt`, label: "English", lang: "en" },
    badge: m === null ? "" : `Recorded ${fmt.utcDate(m.recorded_at)}. ${sleeping ? tail : "The live wall is back."}`,
    caption:
      m?.measured == null
        ? ""
        : `In this recording ${fmt.count(m.measured.gpus, "GPU", "GPUs")} restart: fcvm is back in ${fmt.seconds(m.measured.fcvm_ms)}, EC2 in ${fmt.seconds(m.measured.ec2_ms)}.`,
    play: "Play the recording",
    manualNote: i.reducedMotion ? "Not started by itself, as your device asks for less motion." : "Not started by itself, as your browser asks to save data.",
  };
}
