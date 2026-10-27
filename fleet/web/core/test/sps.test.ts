import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { codecFromSps, nalType, NAL_SPS, splitNals } from "../frame.ts";
import { accessUnitWithNoReorder, nalToRbsp, parseSps, rbspToNal, withNoReorder } from "../sps.ts";

const hex = (h: string) => Uint8Array.from(h.match(/../g)!.map((b) => parseInt(b, 16)));
const toHex = (b: Uint8Array) => Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");

// SPS NAL units from the staging wall's renderer (NVENC, High profile, no
// B-frames), one per level its tile heights select, taken from keyframes of
// `wss://<tile>/frames?h=<h>` on 2026-10-27. None has a bitstream_restriction.
const RENDERER: Record<string, { sps: string; level: number; w: number; h: number }> = {
  h180: { sps: "67640014ac2b202a37f3e02d4040405000003e80000ea60840", level: 20, w: 21, h: 13 },
  h360: { sps: "6764001eac2b2014863f2e02d404040500000303e80000ea6084", level: 30, w: 41, h: 24 },
  h540: { sps: "6764001fac2b201e823fde02d404040500000303e80000ea6084", level: 31, w: 61, h: 35 },
  h720: { sps: "67640020ac2b200a20bb602d4040405000003e80000ea60840", level: 32, w: 81, h: 46 },
  h1080: { sps: "6764002aac2b200f2045fcb80b5010101400000fa00003a98210", level: 42, w: 121, h: 69 },
};

// The rewrites, checked with ffmpeg's trace_headers bitstream filter (an
// independent H.264 syntax reader): bitstream_restriction_flag 1,
// motion_vectors_over_pic_boundaries_flag 1, max_bytes_per_pic_denom 2,
// max_bits_per_mb_denom 1, log2_max_mv_length 15 and 15,
// max_num_reorder_frames 0, max_dec_frame_buffering 3. Decoding a captured
// stream with and without the rewrite gave byte-identical pictures.
const REWRITTEN: Record<string, string> = {
  h180: "67640014ac2b202a37f3e02d4040405000003e80000ea608da08042480",
  h360: "6764001eac2b2014863f2e02d404040500000303e80000ea608da0804248",
  h1080: "6764002aac2b200f2045fcb80b5010101400000fa00003a9823682010920",
};

// x264 (the aggregator's encoder) writes the restriction itself.
const X264_NO_B = "6764000dacb202833f3e0220000003002000000791e28549";
const X264_B3 = "6764000dacd941419f9f0110000003001000000303c0f1429960";
const X264_NAL_HRD = "67640014acb202833f3e022000000300200000079c04001e848003d09c58600f142a48";

// The h360 SPS cut at vui_parameters_present_flag (= 0), and its rewrite.
// ffmpeg's trace_headers reads both; the rewrite as vui_parameters_present_flag
// 1, eight absent VUI parts, then the restriction above.
const NO_VUI = "6764001eac2b2014863f2a";
const NO_VUI_REWRITTEN = "6764001eac2b2014863f2c0368201092";

describe("SPS parse", () => {
  it("reads the renderer's SPS: no bitstream_restriction, so a decoder assumes the level's whole DPB", () => {
    for (const [name, r] of Object.entries(RENDERER)) {
      const s = parseSps(hex(r.sps));
      expect(s, name).not.toBeNull();
      expect(s!.profileIdc).toBe(100);
      expect(s!.constraintSet3).toBe(false);
      expect(s!.levelIdc).toBe(r.level);
      expect(s!.picOrderCntType).toBe(2);
      expect(s!.maxNumRefFrames).toBe(3);
      expect([s!.widthMbs, s!.heightMbs]).toEqual([r.w, r.h]);
      expect(s!.vuiPresent).toBe(true);
      expect(s!.restriction, name).toBeNull();
    }
  });

  it("reads x264's restriction, through HRD parameters and with B-frames", () => {
    expect(parseSps(hex(X264_NO_B))!.restriction).toEqual({ maxNumReorderFrames: 0, maxDecFrameBuffering: 3 });
    expect(parseSps(hex(X264_B3))!.restriction).toEqual({ maxNumReorderFrames: 2, maxDecFrameBuffering: 4 });
    expect(parseSps(hex(X264_B3))!.picOrderCntType).toBe(0);
    expect(parseSps(hex(X264_NAL_HRD))!.restriction).toEqual({ maxNumReorderFrames: 0, maxDecFrameBuffering: 3 });
  });

  it("refuses what is not a whole SPS", () => {
    expect(parseSps(hex("68ee3cb0"))).toBeNull(); // a PPS
    const sps = hex(RENDERER["h360"]!.sps);
    expect(parseSps(sps.subarray(0, 12))).toBeNull();
    // a stray bit after the VUI: syntax this reader does not know
    const extra = hex(RENDERER["h360"]!.sps);
    extra[extra.length - 1] = extra[extra.length - 1]! | 0x40;
    expect(parseSps(extra)).toBeNull();
    expect(withNoReorder(extra)).toBe(extra);
  });
});

describe("withNoReorder", () => {
  it("gives every renderer SPS max_num_reorder_frames 0 and max_dec_frame_buffering max_num_ref_frames", () => {
    for (const [name, r] of Object.entries(RENDERER)) {
      const before = parseSps(hex(r.sps))!;
      const out = withNoReorder(hex(r.sps));
      const after = parseSps(out);
      expect(after, name).not.toBeNull();
      expect(after!.restriction, name).toEqual({ maxNumReorderFrames: 0, maxDecFrameBuffering: before.maxNumRefFrames });
      expect({ ...after!, restriction: null }).toEqual(before);
      // The codec string (profile, constraints, level) is unchanged.
      expect(codecFromSps(out)).toBe(codecFromSps(hex(r.sps)));
    }
    for (const [name, want] of Object.entries(REWRITTEN)) {
      expect(toHex(withNoReorder(hex(RENDERER[name]!.sps))), name).toBe(want);
    }
  });

  it("keeps the emulation prevention the payload needs", () => {
    const out = withNoReorder(hex(RENDERER["h360"]!.sps));
    for (let i = 0; i + 2 < out.length; i++) {
      const bad = out[i] === 0 && out[i + 1] === 0 && out[i + 2]! < 3;
      expect(bad, `00 00 0${out[i + 2]} at byte ${i}`).toBe(false);
    }
    expect(toHex(out)).toContain("00000303e8");
  });

  it("adds a VUI to an SPS that has none", () => {
    expect(parseSps(hex(NO_VUI))!.vuiPresent).toBe(false);
    const out = withNoReorder(hex(NO_VUI));
    expect(toHex(out)).toBe(NO_VUI_REWRITTEN);
    expect(parseSps(out)!.restriction).toEqual({ maxNumReorderFrames: 0, maxDecFrameBuffering: 3 });
  });

  it("leaves an SPS that already has a restriction as it is", () => {
    for (const s of [X264_NO_B, X264_B3, X264_NAL_HRD]) {
      const nal = hex(s);
      expect(withNoReorder(nal)).toBe(nal);
    }
  });

  it("is idempotent", () => {
    const once = withNoReorder(hex(RENDERER["h540"]!.sps));
    expect(withNoReorder(once)).toBe(once);
  });
});

describe("emulation prevention", () => {
  it("round-trips", () => {
    const nal = hex("670000030100000302000003030000030000");
    const rbsp = nalToRbsp(nal);
    expect(toHex(rbsp)).toBe("6700000100000200000300000000");
    expect(toHex(rbspToNal(rbsp))).toBe("670000030100000302000003030000030000");
  });
});

describe("accessUnitWithNoReorder", () => {
  const sc = hex("00000001");
  const cat = (...p: Uint8Array[]) => {
    const out = new Uint8Array(p.reduce((a, x) => a + x.length, 0));
    let o = 0;
    for (const x of p) {
      out.set(x, o);
      o += x.length;
    }
    return out;
  };
  const pps = hex("68ee3cb0");
  const idr = hex("6588840021ffff0000030001");
  const slice = hex("419a2238000003001234");

  it("rewrites the SPS of a keyframe and keeps the other NAL units byte for byte", () => {
    const au = cat(sc, hex(RENDERER["h360"]!.sps), sc, pps, sc, idr);
    const out = accessUnitWithNoReorder(au);
    const nals = splitNals(out);
    expect(nals.map(nalType)).toEqual([NAL_SPS, 8, 5]);
    expect(toHex(nals[0]!)).toBe(REWRITTEN["h360"]);
    expect(toHex(nals[1]!)).toBe(toHex(pps));
    expect(toHex(nals[2]!)).toBe(toHex(idr));
    expect(toHex(out.subarray(0, 4))).toBe("00000001");
  });

  it("handles 3-byte start codes", () => {
    const sc3 = hex("000001");
    const au = cat(sc3, hex(RENDERER["h180"]!.sps), sc3, pps, sc3, idr);
    const nals = splitNals(accessUnitWithNoReorder(au));
    expect(toHex(nals[0]!)).toBe(REWRITTEN["h180"]);
    expect(nals.length).toBe(3);
  });

  it("returns a frame without an SPS untouched, without scanning its slice", () => {
    const au = cat(sc, slice);
    expect(accessUnitWithNoReorder(au)).toBe(au);
    const x264 = cat(sc, hex(X264_NO_B), sc, pps, sc, idr);
    expect(accessUnitWithNoReorder(x264)).toBe(x264);
  });

  it("does not look for an SPS after the first slice", () => {
    const au = cat(sc, slice, sc, hex(RENDERER["h360"]!.sps));
    expect(accessUnitWithNoReorder(au)).toBe(au);
  });
});

it("the frame worker hands the decoder access units with the reorder window removed", () => {
  const src = readFileSync(new URL("../../app/_wall/frames.worker.ts", import.meta.url), "utf8");
  expect(src).toMatch(/new EncodedVideoChunk\(\{[^}]*data: accessUnitWithNoReorder\(f\.data\)/);
});
