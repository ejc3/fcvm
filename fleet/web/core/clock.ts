// Clock sync with the coordinator. The page sends {type:"ping", client_unix_ms}
// and the coordinator answers {type:"clock", server_unix_ms}. With one ping in
// flight at a time, the answer pairs with the outstanding ping (or with the echo,
// when the coordinator sends one). The offset estimate is the one from the sample
// with the smallest round trip among the recent ones, which bounds its error by
// half that round trip.

export interface ClockSample {
  offsetMs: number;
  rttMs: number;
  at: number;
}

export interface ClockState {
  /** client_unix_ms of the ping in flight, or null. */
  outstanding: number | null;
  samples: ClockSample[];
  /** Best estimate of server minus client, ms. Null until the first sample. */
  offsetMs: number | null;
  rttMs: number | null;
}

export const CLOCK_SAMPLES_KEPT = 8;
/** Pings the page sends quickly at start, then one per PING_INTERVAL_MS. */
export const CLOCK_FAST_PINGS = 5;
export const CLOCK_FAST_INTERVAL_MS = 250;
export const PING_INTERVAL_MS = 2000;

export function initialClock(): ClockState {
  return { outstanding: null, samples: [], offsetMs: null, rttMs: null };
}

export function pingSent(c: ClockState, clientUnixMs: number): ClockState {
  return { ...c, outstanding: clientUnixMs };
}

/** Pair a clock message with its ping and update the estimate. `receivedAt` is the local clock. */
export function clockReceived(
  c: ClockState,
  serverUnixMs: number,
  receivedAt: number,
  echo?: number,
): ClockState {
  const sentAt = echo ?? c.outstanding;
  if (sentAt === null || sentAt > receivedAt) return { ...c, outstanding: null };
  const rtt = receivedAt - sentAt;
  const sample: ClockSample = { offsetMs: serverUnixMs - (sentAt + rtt / 2), rttMs: rtt, at: receivedAt };
  const samples = [...c.samples, sample].slice(-CLOCK_SAMPLES_KEPT);
  let best = samples[0]!;
  for (const s of samples) if (s.rttMs < best.rttMs) best = s;
  return { outstanding: null, samples, offsetMs: best.offsetMs, rttMs: best.rttMs };
}

/** The coordinator's time for a local time. Before any sample, the local time. */
export function serverNow(c: ClockState, localNow: number): number {
  return localNow + (c.offsetMs ?? 0);
}

/** How long to wait before the next ping, given how many samples exist. */
export function nextPingDelay(c: ClockState): number {
  return c.samples.length < CLOCK_FAST_PINGS ? CLOCK_FAST_INTERVAL_MS : PING_INTERVAL_MS;
}
