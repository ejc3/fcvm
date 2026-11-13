import type { Metadata } from "next";
import { headers } from "next/headers";
import { NO_FLEET } from "../core/index.ts";
import { pageDirectLiveUrl, pageLiveUrl } from "../core/live-proxy.ts";
import { Phones } from "./_wall/Phones.tsx";
import { Potato } from "./_wall/Potato.tsx";
import { Windows } from "./_wall/Windows.tsx";
import { Minecraft } from "./_wall/Minecraft.tsx";
import { Wall } from "./_wall/Wall.tsx";

// The coordinator's /live URL is read per request, so one build serves any fleet. With the
// coordinator's Access service token set on the Worker, the page tries FLEET_DIRECT_COORD_URL
// first (directUrl: a hostname in the page's own Access application, the faster path) and
// then its own origin, which the Worker forwards (core/live-proxy.ts, worker.ts).
export const dynamic = "force-dynamic";

/** Each mode's tab title, in the server-rendered head: a title set only after hydration was replaced by the layout's. */
const MODE_TITLES: Readonly<Record<string, string>> = { phones: "Phones", potato: "Hot potato", windows: "Windows", minecraft: "Minecraft" };

export async function generateMetadata({ searchParams }: { searchParams: Promise<Record<string, string | string[] | undefined>> }): Promise<Metadata> {
  const { mode } = await searchParams;
  const title = typeof mode === "string" && Object.hasOwn(MODE_TITLES, mode) ? MODE_TITLES[mode] : undefined;
  return title === undefined ? {} : { title };
}

export default async function Page({ searchParams }: { searchParams: Promise<Record<string, string | string[] | undefined>> }) {
  const h = await headers();
  const coordUrl = pageLiveUrl(process.env, h.get("host"), h.get("x-forwarded-proto"));
  const directUrl = coordUrl === process.env.FLEET_COORD_URL ? null : pageDirectLiveUrl(process.env);
  if (coordUrl === "") {
    return <p style={{ padding: 24 }}>{NO_FLEET}</p>;
  }
  // The recording shown while the fleet sleeps: static files beside the page, or elsewhere.
  const mediaUrl = process.env.FLEET_MEDIA_URL ?? "/media";
  // `?mode=phones` shows the phones, `?mode=potato` the potato game, `?mode=windows` the
  // Windows sandboxes and `?mode=minecraft` the Minecraft servers instead of the wall; anything
  // else is the wall.
  const { mode } = await searchParams;
  if (mode === "phones") return <Phones coordUrl={coordUrl} directUrl={directUrl} mediaUrl={mediaUrl} />;
  if (mode === "potato") return <Potato coordUrl={coordUrl} directUrl={directUrl} />;
  if (mode === "windows") return <Windows coordUrl={coordUrl} directUrl={directUrl} />;
  if (mode === "minecraft") return <Minecraft coordUrl={coordUrl} directUrl={directUrl} />;
  return <Wall coordUrl={coordUrl} directUrl={directUrl} mediaUrl={mediaUrl} />;
}
