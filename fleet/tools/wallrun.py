#!/usr/bin/env python3
"""Exercise a running wall through its coordinator: races (`restart_now`) and the
scoreboard, a borrowed GPU (`claim_gpu`: click to the private VM's first frame, its frames,
`claim_camera`, a race during the claim, the return to the wall) and the poster frame. Results go
to OUT as they are taken (jsonl and images), so a box that dies keeps what it had.

    wallrun.py ws://<coordinator>/live OUT RACES

The coordinator must run with a short --restart-cooldown. Needs the renderer's tools
(fleet/renderer/tools) for the frame client, Pillow and the ffmpeg CLI.
"""
import json, os, sys, time, urllib.error, urllib.request
sys.path.insert(0, os.environ.get("FLEETCLIENT_DIR", os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "renderer", "tools")))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import fleetclient as fc  # noqa: E402
import fleetlive  # noqa: E402
from fleetlive import Live, all_running, fcvm_tiles  # noqa: E402
from PIL import Image  # noqa: E402

URL, OUT, RACES = sys.argv[1], sys.argv[2], int(sys.argv[3])
HTTP = "http://" + URL[len("ws://"):].split("/")[0]
os.makedirs(OUT, exist_ok=True)
fleetlive.connect_to(URL)


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)


def append(name, obj):
    with open(os.path.join(OUT, name), "a") as f:
        f.write(json.dumps(obj) + "\n")


def now_ms():
    return int(time.time() * 1000)


def race(v, label):
    v.until(lambda s: (s["cooldown_until"] or 0) < now_ms(), 600)
    r = v.intent("restart_now", {})
    assert r["outcome"] == "committed", r
    with v.cv:
        while v.snap["version"] < r["version"]:
            v.cv.wait(30)
        rid = v.snap["state"]["relaunch_all"]["race"]
    st = v.until(lambda s: any(x["id"] == rid and x["fcvm"]["done_at"] for x in s["races"]), 600)
    ra = [x for x in st["races"] if x["id"] == rid][0]
    h = ra["fcvm"]
    rec = {"label": label, "race": rid, "by": ra["by"], "started_at": ra["started_at"],
           "click_to_last_first_frame_ms": h["time_ms"], "last": h["last"], "measured_on": len(h["tiles"]),
           "dropped": h["dropped"], "sitting_out": h["sitting_out"], "scoreboard": st["scoreboard"],
           "tiles": {sid: {"tile": t["tile"], "click_to_first_frame_ms": t["first_frame_at"] - ra["started_at"],
                           "phases": t["phases"]} for sid, t in sorted(h["tiles"].items())}}
    append("races.jsonl", rec)
    sb = st["scoreboard"]["fcvm"]
    log(f"race {rid} ({label}): {h['time_ms']} ms on {len(h['tiles'])} tiles, sitting out {h['sitting_out']}; "
        f"scoreboard median {sb['median_ms']} ms over {sb['races']} races")
    return rec, st


def grab(frame_url, height, path):
    """One decoded picture from a renderer stream, saved as PNG."""
    hp = frame_url[len("ws://"):].split("/")[0]
    _, p = hp.rsplit(":", 1)
    s = fc.Stream("127.0.0.1", int(p), height)
    hdr, fr, start = None, [], time.time()
    try:
        while time.time() - start < 4:
            op, pl = s.recv()
            if op == 1:
                hdr, fr = json.loads(pl), []
            elif op == 2:
                fr.append(fc.parse_fcvf(pl))
    finally:
        s.close()
    imgs = fc.decode([a for _, a in fr], hdr["width"], hdr["height"])
    Image.frombytes("RGB", (hdr["width"], hdr["height"]), imgs[-1]).save(path)
    return {"width": hdr["width"], "height": hdr["height"], "t": fr[-1][0]["t"], "frames": len(fr)}


def poster(path, after=None, timeout=120):
    """The poster once it exists (and differs from `after`); its headers."""
    end = time.time() + timeout
    while time.time() < end:
        try:
            r = urllib.request.urlopen(HTTP + "/poster.jpg", timeout=10)
            body = r.read()
            if body != after:
                open(path, "wb").write(body)
                return body, {"content_type": r.headers["Content-Type"], "cache_control": r.headers["Cache-Control"],
                              "bytes": len(body), "at": now_ms()}
        except urllib.error.HTTPError as e:
            if e.code != 404:
                raise
        time.sleep(0.5)
    raise TimeoutError("no poster")


v = Live()
st = v.until(lambda s: s["fleet"]["gpus"] > 0, 300)
n = st["fleet"]["gpus"]
st = v.until(lambda s: all_running(s, n), 1800)
log(f"all {n} fcvm tiles running")

for i in range(RACES):
    try:
        time.sleep(15)
        race(v, f"race {i + 1}")
    except Exception as e:  # keep going; the record says what failed
        log(f"race {i + 1} failed: {e!r}")
        append("races.jsonl", {"label": f"race {i + 1}", "error": repr(e)})

v.until(lambda s: all_running(s, n), 600)
body = None
try:
    body, meta = poster(os.path.join(OUT, "poster.jpg"))
    meta["nudges"] = v.snap["state"]["nudges"]
    append("poster.jsonl", meta)
    log(f"poster: {meta['bytes']} bytes, {meta['content_type']}, {meta['cache_control']}, "
        f"holding {meta['nudges']['holding']}")
except Exception as e:
    log(f"poster failed: {e!r}")
    append("poster.jsonl", {"error": repr(e)})

# A visitor's own GPU.
a = Live(client_ip=None)
clicked = now_ms()
r = a.intent("claim_gpu", {})
log("claim_gpu", r["outcome"], r.get("reason"))
append("claim.jsonl", {"event": "receipt", "receipt": r, "clicked_local": clicked})
if r["outcome"] == "committed":
    m = a.message("claim", 300, lambda m: m["claim"]["phase"] == "running")["claim"]
    got = now_ms()
    st = v.until(lambda s: s["claims"] and s["claims"][0]["click_to_first_frame_ms"] is not None, 60)
    c = st["claims"][0]
    borrowed = [t for t in fcvm_tiles(st) if t["id"] == c["borrowed"]][0]
    rec = {"event": "claim", "session": m["id"], "slot": c["slot"], "borrowed": m["tile"], "gpu": m["gpu"],
           "click_to_first_frame_ms": m["first_frame_at"] - m["clicked_at"], "phases": m["phases"],
           "local_click_to_message_ms": got - clicked,
           "tile_phase_for_others": borrowed["phase"], "tile_frame_url_for_others": borrowed["frame_url"],
           "token_in_snapshot": m["token"] in json.dumps(st), "last_relaunch": borrowed["last_relaunch"]}
    append("claim.jsonl", rec)
    log(f"claim {m['id']} on {c['slot']} ({m['tile']}): click to first frame "
        f"{rec['click_to_first_frame_ms']} ms; others see {borrowed['phase']}")
    try:
        append("claim.jsonl", {"event": "frame", **grab(m["frame_url"], 540, os.path.join(OUT, "claim-frame.png"))})
        s2 = a.intent("claim_camera", {"drag": {"yaw_deg": 35.0, "pitch_deg": -5.0, "dive": 2.0}})
        log("claim_camera", s2["outcome"], s2.get("reason"))
        time.sleep(3)
        append("claim.jsonl", {"event": "frame-steered", **grab(m["frame_url"], 540, os.path.join(OUT, "claim-frame-steered.png"))})
    except Exception as e:
        log(f"claim frames failed: {e!r}")
    try:
        rec, _ = race(v, "race during the claim")
    except Exception as e:
        log(f"race during the claim failed: {e!r}")
    try:
        _, pm = poster(os.path.join(OUT, "poster-during-claim.jpg"), after=body, timeout=90)
        pm["nudges"] = v.snap["state"]["nudges"]
        append("poster.jsonl", {"during_claim": True, **pm})
        log("poster during the claim taken")
    except Exception as e:
        log(f"poster during the claim failed: {e!r}")
    left = now_ms()
    a.close()
    want = f"return:{m['id']}"
    st = v.until(lambda s: any(t["id"] == c["borrowed"] and t["last_relaunch"] and t["last_relaunch"]["by"] == want
                               and t["last_relaunch"]["first_frame_at"] for t in fcvm_tiles(s)), 600)
    t = [t for t in fcvm_tiles(st) if t["id"] == c["borrowed"]][0]
    lr = t["last_relaunch"]
    append("claim.jsonl", {"event": "returned", "left_local": left, "clicked_at": lr["clicked_at"],
                           "return_ms": lr["first_frame_at"] - lr["clicked_at"], "phases": lr["phases"]})
    log(f"returned to the wall: {lr['first_frame_at'] - lr['clicked_at']} ms")

json.dump(v.snap["state"], open(os.path.join(OUT, "snapshot-final.json"), "w"), indent=1)
log("WALLRUN_DONE")
