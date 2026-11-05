import { headers } from "next/headers";
import { NO_FLEET } from "../core/index.ts";
import { pageLiveUrl } from "../core/live-proxy.ts";
import { Phones } from "./_wall/Phones.tsx";
import { Potato } from "./_wall/Potato.tsx";
import { Wall } from "./_wall/Wall.tsx";

// The coordinator's /live URL is read per request, so one build serves any fleet. With the
// coordinator's Access service token set on the Worker, it is the page's own origin, which the
// Worker forwards (core/live-proxy.ts, worker.ts).
export const dynamic = "force-dynamic";

export default async function Page({ searchParams }: { searchParams: Promise<Record<string, string | string[] | undefined>> }) {
  const h = await headers();
  const coordUrl = pageLiveUrl(process.env, h.get("host"), h.get("x-forwarded-proto"));
  if (coordUrl === "") {
    return <p style={{ padding: 24 }}>{NO_FLEET}</p>;
  }
  // The recording shown while the fleet sleeps: static files beside the page, or elsewhere.
  const mediaUrl = process.env.FLEET_MEDIA_URL ?? "/media";
  // `?mode=phones` shows the phones and `?mode=potato` the potato game instead of the wall;
  // anything else is the wall.
  const { mode } = await searchParams;
  if (mode === "phones") return <Phones coordUrl={coordUrl} mediaUrl={mediaUrl} />;
  if (mode === "potato") return <Potato coordUrl={coordUrl} />;
  return <Wall coordUrl={coordUrl} mediaUrl={mediaUrl} />;
}
