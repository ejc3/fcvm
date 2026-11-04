// The wall and the phones are two views of one page (`?mode=phones`); both headers carry this
// switch in the same place, on its own line under the title, so either view is one click from the other.

import { useSyncExternalStore } from "react";

export type Mode = "wall" | "phones";

/** The address of `mode` with the rest of the current query kept (`?broadcast=1` and the like). */
export function modeHref(mode: Mode, search: string): string {
  const q = new URLSearchParams(search);
  if (mode === "phones") q.set("mode", "phones");
  else q.delete("mode");
  const s = q.toString();
  return s ? `/?${s}` : "/";
}

const noSubscribe = () => () => {};

export function ModeSwitch({ current }: { current: Mode }) {
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
    </nav>
  );
}
