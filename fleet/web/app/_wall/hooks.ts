"use client";

// Small browser helpers the wall and the phones page share.

import { useCallback, useSyncExternalStore } from "react";
import type { Box } from "../../core/index.ts";

/** An element's box on screen, or null when it has no size. */
export function boxOf(el: Element | null): Box | null {
  if (!el) return null;
  const r = el.getBoundingClientRect();
  if (r.width <= 0 || r.height <= 0) return null;
  return { x: r.left, y: r.top, w: r.width, h: r.height };
}

export function sameBox(a: Box | null, b: Box | null): boolean {
  if (a === null || b === null) return a === b;
  return a.x === b.x && a.y === b.y && a.w === b.w && a.h === b.h;
}

/** Whether a media query matches now, and again whenever it changes. */
export function useMedia(query: string): boolean {
  const subscribe = useCallback(
    (fn: () => void) => {
      const m = window.matchMedia(query);
      m.addEventListener("change", fn);
      return () => m.removeEventListener("change", fn);
    },
    [query],
  );
  return useSyncExternalStore(subscribe, () => window.matchMedia(query).matches, () => false);
}
