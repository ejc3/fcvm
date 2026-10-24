"use client";

// The recording of the wall while the fleet sleeps or wakes. It sits over the
// canvas, which draws the poster (and the live tiles as they arrive) beneath:
// the poster shows until the video plays, and stays if it cannot (a missing
// file) or should not start by itself (less motion asked for, saving data). Once
// the fleet is awake and every region of the live image has a frame, the video
// fades out over it; nothing blank is ever shown in between.

import { useEffect, useRef, useState, type CSSProperties } from "react";
import type { ReelView } from "../../core/index.ts";

/** The crossfade from the recording to the live image (wall.css uses the same). */
const FADE_MS = 700;

export function Reel({ reel, box, onDone }: { reel: ReelView; box: CSSProperties | undefined; onDone: () => void }) {
  const video = useRef<HTMLVideoElement | null>(null);
  const [playing, setPlaying] = useState(false);
  const [failed, setFailed] = useState(false);
  const [started, setStarted] = useState(reel.autoplay);

  // Leaving: the video fades out over the live image (its CSS transition), then
  // goes. A recording that never played has nothing to fade and goes at once.
  useEffect(() => {
    if (!reel.leaving) return;
    const t = setTimeout(onDone, playing && !failed ? FADE_MS : 0);
    return () => clearTimeout(t);
  }, [reel.leaving, playing, failed, onDone]);

  const onSourceError = (last: boolean) => {
    if (last) setFailed(true);
  };

  return (
    <div className={`reel${reel.leaving ? " leaving" : ""}`} style={box} data-testid="reel" data-state={failed ? "failed" : playing ? "playing" : started ? "loading" : "manual"}>
      {failed ? null : (
        <video
          ref={video}
          className={playing ? "on" : undefined}
          muted
          playsInline
          loop
          autoPlay={reel.autoplay}
          preload={reel.autoplay ? "auto" : "none"}
          crossOrigin="anonymous"
          aria-hidden="true"
          data-testid="reel-video"
          onPlaying={() => setPlaying(true)}
          onError={() => setFailed(true)}
        >
          {reel.sources.map((s, i) => (
            <source key={s.src} src={s.src} type={s.type} onError={() => onSourceError(i === reel.sources.length - 1)} />
          ))}
          <track kind="captions" src={reel.track.src} srcLang={reel.track.lang} label={reel.track.label} default />
        </video>
      )}
      {!started && !failed ? (
        // Its own clicks: the stage under it would open the tile beneath.
        <span className="reel-play" onPointerDown={(e) => e.stopPropagation()} onPointerUp={(e) => e.stopPropagation()}>
          <button
            type="button"
            className="btn"
            data-testid="reel-play"
            onClick={() => {
              setStarted(true);
              void video.current?.play().catch(() => setFailed(true));
            }}
          >
            {reel.play}
          </button>
          <span className="plate note">{reel.manualNote}</span>
        </span>
      ) : null}
      {playing && !failed && (reel.badge || reel.caption) ? (
        <span className="reel-badge plate" data-testid="reel-badge">
          {reel.badge}
          {reel.caption ? <span className="caption-line">{reel.caption}</span> : null}
        </span>
      ) : null}
    </div>
  );
}
