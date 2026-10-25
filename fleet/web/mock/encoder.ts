// One H.264 encoder: raw I420 frames in, Annex-B access units out, through an
// ffmpeg child running libx264 (baseline, no B-frames, zero latency).
//
// ffmpeg writes FLV, because FLV frames each access unit with its length: the
// moment a frame's tag is complete it is handed on, with no wait for the next
// frame's start code. The AVC sequence header in the FLV stream carries the SPS
// and PPS, which are put in front of every keyframe so a late joiner can start
// there.

import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { codecFromSps } from "../core/frame.ts";

export interface EncodedUnit {
  keyframe: boolean;
  /** Annex-B access unit. */
  data: Uint8Array;
}

export interface EncoderOptions {
  width: number;
  height: number;
  fps: number;
  /** Keyframe interval, frames. */
  gop: number;
  /** libx264 constant quality (lower is better). */
  crf: number;
  onUnit: (u: EncodedUnit) => void;
  onCodec: (codec: string) => void;
  onExit: (code: number | null, stderr: string) => void;
}

const START = new Uint8Array([0, 0, 0, 1]);

/** The ffmpeg to run: FLEET_FFMPEG, or `ffmpeg` on the PATH. */
export const FFMPEG = process.env.FLEET_FFMPEG ?? "ffmpeg";

function concat(parts: Uint8Array[]): Uint8Array {
  const n = parts.reduce((a, p) => a + p.byteLength, 0);
  const out = new Uint8Array(n);
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.byteLength;
  }
  return out;
}

export class Encoder {
  private child: ChildProcessWithoutNullStreams;
  private buf: Buffer = Buffer.alloc(0);
  private headerDone = false;
  private paramSets: Uint8Array | null = null;
  private stderr = "";
  private closed = false;
  private opts: EncoderOptions;
  readonly width: number;
  readonly height: number;

  constructor(opts: EncoderOptions) {
    this.opts = opts;
    this.width = opts.width;
    this.height = opts.height;
    const args = [
      "-hide_banner", "-loglevel", "error", "-nostdin",
      "-f", "rawvideo", "-pix_fmt", "yuv420p", "-s", `${opts.width}x${opts.height}`, "-r", String(opts.fps),
      "-i", "pipe:0",
      "-an", "-fps_mode", "passthrough",
      "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency", "-profile:v", "baseline",
      "-bf", "0", "-g", String(opts.gop), "-crf", String(opts.crf), "-threads", "1",
      "-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-color_range", "tv",
      "-flush_packets", "1", "-f", "flv", "pipe:1",
    ];
    this.child = spawn(FFMPEG, args, { stdio: ["pipe", "pipe", "pipe"] });
    this.child.stdout.on("data", (d: Buffer) => this.onData(d));
    this.child.stderr.on("data", (d: Buffer) => {
      if (this.stderr.length < 4000) this.stderr += d.toString();
    });
    this.child.stdin.on("error", () => {});
    this.child.on("exit", (code) => {
      this.closed = true;
      opts.onExit(code, this.stderr);
    });
  }

  /** Queue one frame. False when ffmpeg is not keeping up (the frame is dropped). */
  write(i420: Uint8Array): boolean {
    if (this.closed) return false;
    if (this.child.stdin.writableLength > i420.byteLength * 2) return false;
    this.child.stdin.write(i420);
    return true;
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    this.child.stdin.end();
    this.child.kill("SIGKILL");
  }

  private onData(d: Buffer): void {
    this.buf = this.buf.length === 0 ? d : Buffer.concat([this.buf, d]);
    if (!this.headerDone) {
      if (this.buf.length < 13) return;
      // FLV header (9 bytes) and PreviousTagSize0 (4 bytes).
      this.buf = this.buf.subarray(13);
      this.headerDone = true;
    }
    for (;;) {
      if (this.buf.length < 11) return;
      const type = this.buf[0]!;
      const size = this.buf.readUIntBE(1, 3);
      if (this.buf.length < 11 + size + 4) return;
      const body = this.buf.subarray(11, 11 + size);
      this.buf = this.buf.subarray(11 + size + 4);
      if (type === 9) this.onVideoTag(new Uint8Array(body));
    }
  }

  private onVideoTag(body: Uint8Array): void {
    if (body.length < 5) return;
    const keyframe = body[0]! >> 4 === 1;
    const codecId = body[0]! & 0x0f;
    if (codecId !== 7) return;
    const packetType = body[1]!;
    const payload = body.subarray(5);
    if (packetType === 0) {
      // AVCDecoderConfigurationRecord.
      const numSps = payload[5]! & 0x1f;
      let o = 6;
      const sets: Uint8Array[] = [];
      let sps: Uint8Array | null = null;
      for (let i = 0; i < numSps; i++) {
        const len = (payload[o]! << 8) | payload[o + 1]!;
        const nal = payload.subarray(o + 2, o + 2 + len);
        sps ??= nal;
        sets.push(START, nal);
        o += 2 + len;
      }
      const numPps = payload[o]!;
      o += 1;
      for (let i = 0; i < numPps; i++) {
        const len = (payload[o]! << 8) | payload[o + 1]!;
        sets.push(START, payload.subarray(o + 2, o + 2 + len));
        o += 2 + len;
      }
      this.paramSets = concat(sets);
      if (sps) this.opts.onCodec(codecFromSps(sps));
      return;
    }
    if (packetType !== 1) return;
    // Length-prefixed NAL units to Annex-B.
    const parts: Uint8Array[] = [];
    if (keyframe && this.paramSets) parts.push(this.paramSets);
    let o = 0;
    while (o + 4 <= payload.length) {
      const len = ((payload[o]! << 24) | (payload[o + 1]! << 16) | (payload[o + 2]! << 8) | payload[o + 3]!) >>> 0;
      o += 4;
      if (o + len > payload.length) break;
      parts.push(START, payload.subarray(o, o + len));
      o += len;
    }
    this.opts.onUnit({ keyframe, data: concat(parts) });
  }
}
