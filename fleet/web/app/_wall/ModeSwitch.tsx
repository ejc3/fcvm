// The wall, the phones (`?mode=phones`), the potato game (`?mode=potato`), the Windows
// sandboxes (`?mode=windows`) and the Minecraft servers (`?mode=minecraft`) are five views of one
// page; every header carries this switch in the same place, on its own line under the title, so
// each view is one click from the others.

import { useSyncExternalStore } from "react";
import type { StreamModeView } from "../../core/view.ts";

export type Mode = "wall" | "phones" | "potato" | "windows" | "minecraft";

/** The address of `mode` with the rest of the current query kept (`?broadcast=1` and the like). */
export function modeHref(mode: Mode, search: string): string {
  const q = new URLSearchParams(search);
  if (mode === "wall") q.delete("mode");
  else q.set("mode", mode);
  const s = q.toString();
  return s ? `/?${s}` : "/";
}

const noSubscribe = () => () => {};

/**
 * `stream`: how the wall's picture arrives, after the two links. With `onStream` it is a button
 * that opens the panel holding the stream switches; without one (no controls to open), plain text.
 */
export function ModeSwitch({ current, stream = null, onStream }: { current: Mode; stream?: StreamModeView | null; onStream?: (from: HTMLElement) => void }) {
  // The server renders the plain addresses; the client then adds the rest of its query.
  const search = useSyncExternalStore(noSubscribe, () => window.location.search, () => "");
  const item = (mode: Mode, label: string) => (
    <a className="mode-link" href={modeHref(mode, search)} aria-current={mode === current ? "page" : undefined} data-testid={`mode-${mode}`}>
      {label}
    </a>
  );
  return (
    <nav className="mode-switch" aria-label="View" data-hud-box="">
      {item("wall", "Wall")}
      {item("phones", "Phones")}
      {item("potato", "Potato")}
      {item("windows", "Windows")}
      {item("minecraft", "Minecraft")}
      <a className="mode-link" href="/boot-race" data-testid="mode-boot-race" title="Recorded results of the boot-race campaign">
        Boot race
      </a>
      {stream === null ? null : onStream ? (
        <button type="button" className="stream-mode" title={stream.detail} aria-label={`${stream.label}: ${stream.detail}`} data-testid="stream-mode" onClick={(e) => onStream(e.currentTarget)}>
          {stream.label}
        </button>
      ) : (
        <span className="stream-mode" title={stream.detail} data-testid="stream-mode">
          {stream.label}
        </span>
      )}
    </nav>
  );
}
