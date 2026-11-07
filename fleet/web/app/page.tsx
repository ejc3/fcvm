import { headers } from "next/headers";
import { NO_FLEET } from "../core/index.ts";
import { pageDirectLiveUrl, pageLiveUrl } from "../core/live-proxy.ts";
import { Phones } from "./_wall/Phones.tsx";
import { Potato } from "./_wall/Potato.tsx";
import { Wall } from "./_wall/Wall.tsx";

// The coordinator's /live URL is read per request, so one build serves any fleet. With the
// coordinator's Access service token set on the Worker, the page tries the browser's direct
// host first (directUrl) and then its own origin, which the Worker forwards.
export const dynamic = "force-dynamic";

export default async function Page({ searchParams }: { searchParams: Promise<Record<string, string | string[] | undefined>> }) {
  const h = await headers();
  const coordUrl = pageLiveUrl(process.env, h.get("host"), h.get("x-forwarded-proto"));
  const directUrl = coordUrl === process.env.FLEET_COORD_URL ? null : pageDirectLiveUrl(process.env);
  if (coordUrl === "") {
    return <p style={{ padding: 24 }}>{NO_FLEET}</p>;
  }
  // The recording shown while the fleet sleeps: static files beside the page, or elsewhere.
  const mediaUrl = process.env.FLEET_MEDIA_URL ?? "/media";
  // `?mode=phones` shows the phones and `?mode=potato` the potato game instead of the wall;
  // anything else is the wall.
  const { mode } = await searchParams;
  if (mode === "phones") return <Phones coordUrl={coordUrl} directUrl={directUrl} mediaUrl={mediaUrl} />;
  if (mode === "potato") return <Potato coordUrl={coordUrl} directUrl={directUrl} />;
  return <Wall coordUrl={coordUrl} directUrl={directUrl} mediaUrl={mediaUrl} />;
}
