import { NO_FLEET } from "../core/index.ts";
import { Wall } from "./_wall/Wall.tsx";

// The coordinator's /live URL is read per request, so one build serves any fleet.
export const dynamic = "force-dynamic";

export default function Page() {
  const coordUrl = process.env.FLEET_COORD_URL ?? "";
  if (coordUrl === "") {
    return <p style={{ padding: 24 }}>{NO_FLEET}</p>;
  }
  // The recording shown while the fleet sleeps: static files beside the page, or elsewhere.
  const mediaUrl = process.env.FLEET_MEDIA_URL ?? "/media";
  return <Wall coordUrl={coordUrl} mediaUrl={mediaUrl} />;
}
