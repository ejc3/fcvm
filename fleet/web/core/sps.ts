// H.264 sequence parameter sets: the fields that decide how many frames a
// decoder holds before it outputs one, and a rewrite that declares none.
//
// A decoder outputs a frame only once no later-decoded frame can come before it
// in display order. Without the VUI's bitstream_restriction, the SPS does not
// say how many that is, and a decoder must assume the level's MaxDpbFrames:
// Chromium's H264Decoder (which VideoToolboxVideoDecoder drives on macOS)
// sets its reorder window to dpb_.max_num_pics() then,
// unless profile_idc is 44/86/100/110/122/244 with constraint_set3_flag set
// (H264Decoder::UpdateMaxNumReorderFrames, media/gpu/h264_decoder.cc). Every
// decoded frame then waits for that many more to be decoded: 7 or 8 frames,
// 234 to 268 ms at 30 fps, on the wall's tiles.
//
// The frame protocol carries no B-frames (fleet/PROTOCOL.md, "H.264 High
// profile, no B-frames"), so output order is decode order and
// max_num_reorder_frames = 0 is true of every stream. withNoReorder writes it.

/** What an SPS says about the decoded picture buffer. */
export interface SpsInfo {
  profileIdc: number;
  constraintSet3: boolean;
  levelIdc: number;
  picOrderCntType: number;
  maxNumRefFrames: number;
  /** Coded size in macroblocks. */
  widthMbs: number;
  heightMbs: number;
  vuiPresent: boolean;
  /** The VUI's bitstream_restriction, or null when the SPS has none. */
  restriction: { maxNumReorderFrames: number; maxDecFrameBuffering: number } | null;
}

/** Strip emulation prevention bytes: 00 00 03 becomes 00 00. */
export function nalToRbsp(nal: Uint8Array): Uint8Array {
  const out = new Uint8Array(nal.length);
  let n = 0;
  let zeros = 0;
  for (let i = 0; i < nal.length; i++) {
    const b = nal[i]!;
    if (zeros >= 2 && b === 3) {
      zeros = 0;
      continue;
    }
    out[n++] = b;
    zeros = b === 0 ? zeros + 1 : 0;
  }
  return out.subarray(0, n);
}

/** Insert emulation prevention bytes so no 00 00 0x (x <= 3) appears. */
export function rbspToNal(rbsp: Uint8Array): Uint8Array {
  const out: number[] = [];
  let zeros = 0;
  for (const b of rbsp) {
    if (zeros >= 2 && b <= 3) {
      out.push(3);
      zeros = 0;
    }
    out.push(b);
    zeros = b === 0 ? zeros + 1 : 0;
  }
  return Uint8Array.from(out);
}

class BitReader {
  pos = 0;
  private readonly buf: Uint8Array;
  constructor(buf: Uint8Array) {
    this.buf = buf;
  }
  get bits(): number {
    return this.buf.length * 8;
  }
  u1(): number {
    if (this.pos >= this.bits) throw new RangeError("SPS ends early");
    const b = (this.buf[this.pos >> 3]! >> (7 - (this.pos & 7))) & 1;
    this.pos++;
    return b;
  }
  u(n: number): number {
    let v = 0;
    for (let i = 0; i < n; i++) v = v * 2 + this.u1();
    return v;
  }
  ue(): number {
    let lz = 0;
    while (this.u1() === 0) {
      if (++lz > 31) throw new RangeError("exp-Golomb code too long");
    }
    return 2 ** lz - 1 + this.u(lz);
  }
  se(): number {
    const k = this.ue();
    return k % 2 === 1 ? (k + 1) / 2 : -(k / 2);
  }
}

class BitWriter {
  private readonly out: number[] = [];
  private cur = 0;
  private n = 0;
  u1(b: number): void {
    this.cur = (this.cur << 1) | (b & 1);
    if (++this.n === 8) {
      this.out.push(this.cur);
      this.cur = 0;
      this.n = 0;
    }
  }
  u(n: number, v: number): void {
    for (let i = n - 1; i >= 0; i--) this.u1(Math.floor(v / 2 ** i) & 1);
  }
  ue(v: number): void {
    const x = v + 1;
    const len = Math.floor(Math.log2(x));
    this.u(len, 0);
    this.u(len + 1, x);
  }
  /** rbsp_trailing_bits: a 1, then 0s to the byte boundary. */
  trailing(): Uint8Array {
    this.u1(1);
    while (this.n !== 0) this.u1(0);
    return Uint8Array.from(this.out);
  }
}

const HIGH_PROFILES = new Set([100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135]);

function skipScalingList(r: BitReader, size: number): void {
  let last = 8;
  let next = 8;
  for (let j = 0; j < size; j++) {
    if (next !== 0) {
      next = (last + r.se() + 256) % 256;
    }
    last = next === 0 ? last : next;
  }
}

function skipHrd(r: BitReader): void {
  const cpbCnt = r.ue() + 1;
  r.u(8); // bit_rate_scale, cpb_size_scale
  for (let i = 0; i < cpbCnt; i++) {
    r.ue();
    r.ue();
    r.u1();
  }
  r.u(20); // four 5-bit lengths
}

interface Parsed {
  info: SpsInfo;
  /** Bit offset in the RBSP of bitstream_restriction_flag, or of vui_parameters_present_flag when there is no VUI. */
  editAt: number;
}

/** The bit offset of rbsp_stop_one_bit: the last 1 bit of the RBSP. */
function stopBit(rbsp: Uint8Array): number {
  for (let i = rbsp.length - 1; i >= 0; i--) {
    const b = rbsp[i]!;
    if (b !== 0) {
      let k = 0;
      while (((b >> k) & 1) === 0) k++;
      return i * 8 + (7 - k);
    }
  }
  return -1;
}

function parse(rbsp: Uint8Array): Parsed | null {
  if (rbsp.length < 4 || (rbsp[0]! & 0x1f) !== 7) return null;
  const r = new BitReader(rbsp);
  try {
    r.u(8); // NAL header
    const profileIdc = r.u(8);
    const constraints = r.u(8);
    const levelIdc = r.u(8);
    r.ue(); // seq_parameter_set_id
    if (HIGH_PROFILES.has(profileIdc)) {
      const chroma = r.ue();
      if (chroma === 3) r.u1();
      r.ue();
      r.ue();
      r.u1();
      if (r.u1()) {
        for (let i = 0; i < (chroma !== 3 ? 8 : 12); i++) {
          if (r.u1()) skipScalingList(r, i < 6 ? 16 : 64);
        }
      }
    }
    r.ue(); // log2_max_frame_num_minus4
    const picOrderCntType = r.ue();
    if (picOrderCntType === 0) r.ue();
    else if (picOrderCntType === 1) {
      r.u1();
      r.se();
      r.se();
      const n = r.ue();
      for (let i = 0; i < n; i++) r.se();
    }
    const maxNumRefFrames = r.ue();
    r.u1(); // gaps_in_frame_num_allowed_flag
    const widthMbs = r.ue() + 1;
    const heightMapUnits = r.ue() + 1;
    const frameMbsOnly = r.u1();
    if (!frameMbsOnly) r.u1();
    r.u1(); // direct_8x8_inference_flag
    if (r.u1()) {
      r.ue();
      r.ue();
      r.ue();
      r.ue();
    }
    const vuiAt = r.pos;
    const vuiPresent = r.u1() === 1;
    let editAt = vuiAt;
    let restriction: SpsInfo["restriction"] = null;
    if (vuiPresent) {
      if (r.u1() && r.u(8) === 255) r.u(32);
      if (r.u1()) r.u1();
      if (r.u1()) {
        r.u(4);
        if (r.u1()) r.u(24);
      }
      if (r.u1()) {
        r.ue();
        r.ue();
      }
      if (r.u1()) {
        r.u(32);
        r.u(32);
        r.u1();
      }
      const nalHrd = r.u1();
      if (nalHrd) skipHrd(r);
      const vclHrd = r.u1();
      if (vclHrd) skipHrd(r);
      if (nalHrd || vclHrd) r.u1();
      r.u1(); // pic_struct_present_flag
      editAt = r.pos;
      if (r.u1()) {
        r.u1();
        r.ue();
        r.ue();
        r.ue();
        r.ue();
        restriction = { maxNumReorderFrames: r.ue(), maxDecFrameBuffering: r.ue() };
      }
    }
    // Everything after must be the trailing bits, or this SPS has syntax this
    // reader does not know and must not be edited.
    if (r.pos !== stopBit(rbsp)) return null;
    return {
      info: {
        profileIdc,
        constraintSet3: (constraints & 0x10) !== 0,
        levelIdc,
        picOrderCntType,
        maxNumRefFrames,
        widthMbs,
        heightMbs: heightMapUnits * (frameMbsOnly ? 1 : 2),
        vuiPresent,
        restriction,
      },
      editAt,
    };
  } catch {
    return null;
  }
}

/** The SPS fields above, from an SPS NAL unit (header byte included, no start code); null if it does not parse. */
export function parseSps(nal: Uint8Array): SpsInfo | null {
  return parse(nalToRbsp(nal))?.info ?? null;
}

/**
 * The SPS with a VUI bitstream_restriction saying no frame is reordered:
 * max_num_reorder_frames 0 and max_dec_frame_buffering max_num_ref_frames, the
 * least the standard allows (and Chromium's parser checks). The other
 * restriction fields get the values the standard infers when they are absent.
 * An SPS that already has a restriction, or that does not parse, comes back
 * unchanged.
 */
export function withNoReorder(nal: Uint8Array): Uint8Array {
  const rbsp = nalToRbsp(nal);
  const p = parse(rbsp);
  if (!p || p.info.restriction) return nal;
  const w = new BitWriter();
  const head = new BitReader(rbsp);
  while (head.pos < p.editAt) w.u1(head.u1());
  if (!p.info.vuiPresent) {
    w.u1(1); // vui_parameters_present_flag
    // aspect_ratio_info, overscan_info, video_signal_type, chroma_loc_info,
    // timing_info, nal_hrd, vcl_hrd, pic_struct: none present
    w.u(8, 0);
  }
  w.u1(1); // bitstream_restriction_flag
  w.u1(1); // motion_vectors_over_pic_boundaries_flag (inferred 1)
  w.ue(2); // max_bytes_per_pic_denom (inferred 2)
  w.ue(1); // max_bits_per_mb_denom (inferred 1)
  w.ue(15); // log2_max_mv_length_horizontal (inferred 15)
  w.ue(15); // log2_max_mv_length_vertical (inferred 15)
  w.ue(0); // max_num_reorder_frames
  w.ue(p.info.maxNumRefFrames); // max_dec_frame_buffering
  return rbspToNal(w.trailing());
}

/**
 * An Annex-B access unit with each SPS passed through withNoReorder. Only the
 * NAL units before the first slice are scanned (an SPS cannot follow one in an
 * access unit), so a frame without one costs a few bytes of scanning. Returns
 * the input when nothing changed.
 */
export function accessUnitWithNoReorder(au: Uint8Array): Uint8Array {
  const parts: Uint8Array[] = [];
  let copiedTo = 0;
  let i = 0;
  let changed = false;
  for (;;) {
    // the next start code at or after i
    let sc = -1;
    let scLen = 0;
    for (let k = i; k + 2 < au.length; k++) {
      if (au[k] === 0 && au[k + 1] === 0 && au[k + 2] === 1) {
        sc = k;
        scLen = 3;
        break;
      }
    }
    if (sc < 0) break;
    const begin = sc + scLen;
    if (begin >= au.length) break;
    const type = au[begin]! & 0x1f;
    if (type >= 1 && type <= 5) break;
    // its end: the next start code (00 00 01, with a leading 00 of a 4-byte one trimmed)
    let end = au.length;
    for (let k = begin; k + 2 < au.length; k++) {
      if (au[k] === 0 && au[k + 1] === 0 && au[k + 2] === 1) {
        end = k;
        break;
      }
    }
    let nalEnd = end;
    while (nalEnd > begin && au[nalEnd - 1] === 0) nalEnd--;
    if (type === 7) {
      const sps = au.subarray(begin, nalEnd);
      const fixed = withNoReorder(sps);
      if (fixed !== sps) {
        parts.push(au.subarray(copiedTo, begin), fixed);
        copiedTo = nalEnd;
        changed = true;
      }
    }
    i = end;
  }
  if (!changed) return au;
  parts.push(au.subarray(copiedTo));
  const out = new Uint8Array(parts.reduce((a, p) => a + p.length, 0));
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}
