// Decisions for one frame socket and its decoder, kept out of the worker so
// they can be tested.

/** Chunks the decoder may hold undecoded before the page starts dropping. */
export const MAX_DECODE_QUEUE = 8;

export interface StreamCtl {
  /** The height in the URL the socket was opened with. */
  openedHeight: number;
  /** The height last sent as {"h"} on this socket, if any. */
  sentHeight: number | null;
  /** The height the page wants now. */
  wanted: number;
  open: boolean;
}

export function newStream(height: number): StreamCtl {
  return { openedHeight: height, sentHeight: null, wanted: height, open: false };
}

export function streamOpened(s: StreamCtl, open: boolean): StreamCtl {
  return { ...s, open };
}

export function wantHeight(s: StreamCtl, height: number): StreamCtl {
  return { ...s, wanted: height };
}

/** The height to send now, or null: the socket is not open, or already has it. */
export function heightToSend(s: StreamCtl): number | null {
  if (!s.open) return null;
  return s.wanted === (s.sentHeight ?? s.openedHeight) ? null : s.wanted;
}

export function heightSent(s: StreamCtl, height: number): StreamCtl {
  return { ...s, sentHeight: height };
}

/**
 * Whether to decode a chunk. A decoder that is backed up drops chunks; once a
 * chunk is dropped, deltas cannot decode, so it waits for the next keyframe.
 */
export function admitChunk(
  s: { waitingKey: boolean },
  c: { keyframe: boolean; queueSize: number },
): { decode: boolean; waitingKey: boolean } {
  if (c.queueSize > MAX_DECODE_QUEUE) return { decode: false, waitingKey: true };
  if (s.waitingKey && !c.keyframe) return { decode: false, waitingKey: true };
  return { decode: true, waitingKey: false };
}
