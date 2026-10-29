// Messages between the phones page and the GPU mirror's worker (mirror.worker.ts).

export type MirrorIn =
  | { type: "canvas"; canvas: OffscreenCanvas }
  /** Open the phone streamer's GET /mirror with the viewer's direct token. */
  | { type: "open"; url: string; admit: string }
  | { type: "close" }
  /** The replay test: a captured tap stream, as the streamer's socket gives it before compression. */
  | { type: "replay"; bytes: ArrayBuffer };

export interface MirrorStatsMsg {
  type: "stats";
  at: number;
  /** WebSocket message bytes per second over the last second, and in all since the stream opened. */
  wireBps: number;
  wireBytes: number;
  /** Bytes of the tap stream after decoding. */
  tapBytes: number;
  /** The stream's encoding (null before its first message). */
  codec: "zstd" | "deflate-raw" | null;
  /** Milliseconds a second spent decoding zstd (null when not zstd). */
  decodeMsPerSec: number | null;
  /** Blocking calls the phone's GPU answered per second (each a round trip the viewer did not make). */
  repliesPerSec: number;
  /** Frames posted to the screen per second. */
  fps: number;
  posts: number;
  calls: number;
  skipped: number;
  failed: number;
  unknownChannelPackets: number;
  overflowed: boolean;
  open: boolean;
  /** The centre pixel of the last frame posted (RGBA), so a test can see what the mirror drew. */
  pixel: number[] | null;
  /** The last stamp read from a posted frame (Robot Park's frame number k, scene time k/60 s), and when (local ms). */
  stamp: { k: number; at: number } | null;
  /** The mirror's picture (the screen it last drew), for the page's layout of a turned phone. */
  width: number;
  height: number;
}

export type MirrorOut =
  | MirrorStatsMsg
  /** The tap's HELLO arrived: the copy runs, and the app can be restarted into it. */
  | { type: "joined" }
  | { type: "closed"; code: number; reason: string }
  /** The canvas has WebGL2: the mirror can open. */
  | { type: "ready" }
  /** It has not: the page keeps the video. */
  | { type: "unavailable"; reason: string };
