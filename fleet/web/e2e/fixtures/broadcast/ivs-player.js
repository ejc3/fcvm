// A stand-in for the Amazon IVS web player, served by the tests in place of the real one.
// It plays a picture of the wall drawn on a canvas: every tile one flat colour
// (window.__fakeBroadcast.colorOf), as the wall was BROADCAST_DELAY_MS ago, with that
// moment's scene time t and epoch e in an SEI for each presented frame, as the broadcast
// aggregator stamps its frames. Needs the page's debug hook (?debug=1) for the layout and clock.
(() => {
  const BROADCAST_DELAY_MS = 1800;
  const W = 960;
  const H = 540;
  /** A colour per tile, far apart for neighbouring ids. */
  const colorOf = (id) => {
    let h = 0;
    for (const ch of id) h = (h * 131 + ch.charCodeAt(0)) >>> 0;
    return [40 + (h % 7) * 30, 40 + ((h >> 3) % 7) * 30, 40 + ((h >> 6) % 7) * 30];
  };
  window.__fakeBroadcast = { colorOf, frames: 0, delayMs: BROADCAST_DELAY_MS };

  class FakePlayer {
    constructor() {
      this.listeners = new Map();
      this.video = null;
      this.timer = null;
      this.last = null;
    }
    attachHTMLVideoElement(v) {
      this.video = v;
    }
    addEventListener(name, fn) {
      if (!this.listeners.has(name)) this.listeners.set(name, []);
      this.listeners.get(name).push(fn);
    }
    emit(name, payload) {
      for (const fn of this.listeners.get(name) ?? []) fn(payload);
    }
    setMuted() {}
    load() {
      const c = document.createElement("canvas");
      c.width = W;
      c.height = H;
      const g = c.getContext("2d");
      const draw = () => {
        const f = window.__fleet;
        const s = f?.client.getState();
        if (!s?.accepted) return;
        const wall = performance.timeOrigin + performance.now() + (s.clock.offsetMs ?? 0) - BROADCAST_DELAY_MS;
        let st = s.accepted;
        let best = -Infinity;
        for (const x of s.history) if (x.at <= wall && x.at > best) [best, st] = [x.at, x.state];
        g.fillStyle = "#000";
        g.fillRect(0, 0, W, H);
        for (const t of st.tiles) {
          const [r, gg, b] = colorOf(t.id);
          g.fillStyle = `rgb(${r},${gg},${b})`;
          g.fillRect(Math.round(t.rect.x * W), Math.round(t.rect.y * H), Math.round(t.rect.w * W), Math.round(t.rect.h * H));
        }
        const e = st.scene.epoch_unix_ms;
        this.last = { t: (wall - e) / 1000, e };
        window.__fakeBroadcast.frames++;
      };
      draw();
      this.timer = setInterval(draw, 33);
      const v = this.video;
      v.srcObject = c.captureStream(30);
      const onPresented = (_now, md) => {
        if (this.last) this.emit(window.IVSPlayer.MetadataEventType.SEI, { timestamp: md.mediaTime, data: new TextEncoder().encode(JSON.stringify(this.last)) });
        if (this.timer !== null) v.requestVideoFrameCallback(onPresented);
      };
      v.requestVideoFrameCallback(onPresented);
      v.play().then(() => this.emit(window.IVSPlayer.PlayerState.PLAYING), () => {});
    }
    play() {
      void this.video?.play().catch(() => {});
    }
    pause() {
      this.video?.pause();
    }
    delete() {
      if (this.timer !== null) clearInterval(this.timer);
      this.timer = null;
    }
  }

  window.IVSPlayer = {
    isPlayerSupported: true,
    PlayerState: { PLAYING: "Playing", BUFFERING: "Buffering", ENDED: "Ended", IDLE: "Idle" },
    PlayerEventType: { ERROR: "PlayerError" },
    MetadataEventType: { SEI: "PlayerSeiEvent" },
    create: () => new FakePlayer(),
  };
})();
