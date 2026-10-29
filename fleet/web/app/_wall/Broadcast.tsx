"use client";

// The broadcast player: Amazon IVS's web player on a <video> over the image. Every frame's SEI
// names its scene time t; the IVS player raises it with the frame's media time, and each
// presented frame (requestVideoFrameCallback) is matched to it, so the client knows the t of
// the frame on screen and draws the overlay for that moment (core/broadcast.ts).

import { useEffect, useRef, useState } from "react";
import { FrameMatcher, memoUntilFailure, type ShownFrame } from "../../core/index.ts";

/** The IVS web player, pinned. */
export const IVS_PLAYER_URL = "https://player.live-video.net/1.57.0/amazon-ivs-player.min.js";
/** After an error or the end of the stream, the player is loaded again this much later. */
const RETRY_MS = 3000;

interface IvsPlayer {
  attachHTMLVideoElement(v: HTMLVideoElement): void;
  addEventListener(name: string, fn: (payload: unknown) => void): void;
  load(url: string): void;
  play(): void;
  pause(): void;
  setMuted(m: boolean): void;
  delete(): void;
}

interface IvsGlobal {
  create(): IvsPlayer;
  isPlayerSupported: boolean;
  PlayerState: { PLAYING: string; BUFFERING: string; ENDED: string; IDLE: string };
  PlayerEventType: { ERROR: string };
  MetadataEventType: { SEI: string };
}

declare global {
  interface Window {
    IVSPlayer?: IvsGlobal;
  }
}

/** The IVS player script, loaded once; a failed load (no IVSPlayer, a network error) is tried again. */
const loadPlayer = memoUntilFailure(
  () =>
    new Promise<IvsGlobal>((resolve, reject) => {
      if (window.IVSPlayer) return resolve(window.IVSPlayer);
      const s = document.createElement("script");
      s.src = IVS_PLAYER_URL;
      s.async = true;
      s.onload = () => {
        s.remove();
        if (window.IVSPlayer) resolve(window.IVSPlayer);
        else reject(new Error("no IVSPlayer"));
      };
      s.onerror = () => {
        s.remove();
        reject(new Error("the IVS player did not load"));
      };
      document.head.appendChild(s);
    }),
);

/** The frame time an SEI carries: {"t": scene seconds, "e": scene epoch ms}. */
function seiFrame(data: unknown): { t: number; e: number } | null {
  try {
    const bytes = data instanceof Uint8Array ? data : data instanceof ArrayBuffer ? new Uint8Array(data) : null;
    if (!bytes) return null;
    const m = JSON.parse(new TextDecoder().decode(bytes)) as { t?: unknown; e?: unknown };
    return typeof m.t === "number" && typeof m.e === "number" ? { t: m.t, e: m.e } : null;
  } catch {
    return null;
  }
}

export type BroadcastStatus = "loading" | "playing" | "buffering" | "reconnecting" | "unsupported";

export function Broadcast({
  url,
  paused,
  box,
  onFrame,
}: {
  url: string;
  /** The page's Pause: the picture stops (the player holds its frame). */
  paused: boolean;
  box: { left: number; top: number; width: number; height: number };
  onFrame: (f: ShownFrame) => void;
}) {
  const videoRef = useRef<HTMLVideoElement | null>(null);
  // The newest URL (its token is renewed every few minutes); a reload takes it, a renewal alone does not.
  const urlRef = useRef(url);
  urlRef.current = url;
  const onFrameRef = useRef(onFrame);
  onFrameRef.current = onFrame;
  const pausedRef = useRef(paused);
  pausedRef.current = paused;
  const playerRef = useRef<IvsPlayer | null>(null);
  const [status, setStatus] = useState<BroadcastStatus>("loading");
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    const video = videoRef.current;
    if (!video) return;
    let gone = false;
    let player: IvsPlayer | null = null;
    let retry: ReturnType<typeof setTimeout> | null = null;
    let callback = 0;
    const matcher = new FrameMatcher();
    const again = () => {
      if (gone || retry) return;
      setStatus("reconnecting");
      retry = setTimeout(() => setAttempt((a) => a + 1), RETRY_MS);
    };
    const onPresented = (_now: number, md: VideoFrameCallbackMetadata) => {
      const f = matcher.presented(Math.round(md.mediaTime * 1000), performance.timeOrigin + md.expectedDisplayTime);
      if (f) onFrameRef.current(f);
      callback = video.requestVideoFrameCallback(onPresented);
    };
    void loadPlayer()
      .then((IVS) => {
        if (gone) return;
        if (!IVS.isPlayerSupported) {
          setStatus("unsupported");
          return;
        }
        const p = IVS.create();
        player = p;
        playerRef.current = p;
        p.attachHTMLVideoElement(video);
        p.setMuted(true);
        p.addEventListener(IVS.PlayerState.PLAYING, () => setStatus("playing"));
        p.addEventListener(IVS.PlayerState.BUFFERING, () => setStatus("buffering"));
        p.addEventListener(IVS.PlayerState.ENDED, again);
        p.addEventListener(IVS.PlayerEventType.ERROR, again);
        p.addEventListener(IVS.MetadataEventType.SEI, (ev) => {
          const m = ev as { timestamp?: unknown; data?: unknown };
          const f = seiFrame(m.data);
          if (!f || typeof m.timestamp !== "number") return;
          for (const shown of matcher.sei(Math.round(m.timestamp * 1000), f)) onFrameRef.current(shown);
          // Without frame callbacks the SEI's own moment stands in for the frame's.
          if (!("requestVideoFrameCallback" in video)) onFrameRef.current({ ...f, at: performance.timeOrigin + performance.now() });
        });
        if ("requestVideoFrameCallback" in video) callback = video.requestVideoFrameCallback(onPresented);
        p.load(urlRef.current);
        if (!pausedRef.current) p.play();
      })
      .catch(again);
    return () => {
      gone = true;
      if (retry) clearTimeout(retry);
      if (callback && "cancelVideoFrameCallback" in video) video.cancelVideoFrameCallback(callback);
      player?.delete();
      playerRef.current = null;
    };
  }, [attempt]);

  // The page's Pause and Resume.
  useEffect(() => {
    const p = playerRef.current;
    if (!p) return;
    if (paused) p.pause();
    else p.play();
  }, [paused]);

  const note =
    status === "reconnecting"
      ? "The broadcast is reconnecting."
      : status === "buffering"
        ? "The broadcast is buffering."
        : status === "unsupported"
          ? "This browser cannot play the broadcast."
          : null;
  return (
    <div className="broadcast" style={box} data-testid="broadcast" data-status={status}>
      <video ref={videoRef} playsInline muted autoPlay aria-label="The broadcast of the whole wall" />
      {note ? <p className="broadcast-note" role="status">{note}</p> : null}
    </div>
  );
}
