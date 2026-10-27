#include "nvenc.h"

#include <dlfcn.h>
#include <stdlib.h>

#include <cstdio>
#include <cstring>
#include <mutex>
#include <algorithm>
#include <iterator>
#include <vector>

#include "ffnvcodec/nvEncodeAPI.h"

namespace {
NV_ENCODE_API_FUNCTION_LIST g_fl;
bool g_loaded = false;
std::string g_load_err;

bool load(std::string* err) {
  static std::once_flag once;
  std::call_once(once, [] {
    void* lib = dlopen("libnvidia-encode.so.1", RTLD_NOW);
    if (!lib) { g_load_err = std::string("dlopen libnvidia-encode.so.1: ") + dlerror(); return; }
    typedef NVENCSTATUS (*MaxVer)(uint32_t*);
    typedef NVENCSTATUS (*Create)(NV_ENCODE_API_FUNCTION_LIST*);
    auto maxver = (MaxVer)dlsym(lib, "NvEncodeAPIGetMaxSupportedVersion");
    auto create = (Create)dlsym(lib, "NvEncodeAPICreateInstance");
    if (!maxver || !create) { g_load_err = "libnvidia-encode lacks the NVENC entry points"; return; }
    uint32_t v = 0;
    maxver(&v);
    uint32_t need = (NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION;
    if (v < need) {
      char b[160];
      snprintf(b, sizeof b, "driver supports NVENC API %u.%u, this build needs %u.%u", v >> 4, v & 15,
               NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION);
      g_load_err = b;
      return;
    }
    memset(&g_fl, 0, sizeof g_fl);
    g_fl.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    if (create(&g_fl) != NV_ENC_SUCCESS) { g_load_err = "NvEncodeAPICreateInstance failed"; return; }
    g_loaded = true;
  });
  if (!g_loaded) *err = g_load_err;
  return g_loaded;
}

std::string st(const char* what, NVENCSTATUS s) {
  char b[128];
  snprintf(b, sizeof b, "%s failed: NVENCSTATUS %d", what, (int)s);
  return b;
}
}  // namespace

long find_sps(const uint8_t* p, size_t n, size_t* len) {
  for (size_t i = 0; i + 3 < n; i++) {
    size_t sc = 0;
    if (p[i] == 0 && p[i + 1] == 0 && p[i + 2] == 1) sc = 3;
    else if (i + 4 < n && p[i] == 0 && p[i + 1] == 0 && p[i + 2] == 0 && p[i + 3] == 1) sc = 4;
    if (!sc) continue;
    size_t s = i + sc;
    if (s < n && (p[s] & 0x1f) == 7) {
      size_t e = s + 1;
      while (e + 2 < n && !(p[e] == 0 && p[e + 1] == 0 && (p[e + 2] == 1 || (e + 3 < n && p[e + 2] == 0 && p[e + 3] == 1)))) e++;
      if (e + 2 >= n) e = n;
      *len = e - s;
      return (long)s;
    }
    i = s - 1;
  }
  return -1;
}

namespace {
struct Bits {
  std::vector<uint8_t> b;  // RBSP: emulation prevention bytes removed
  size_t pos = 0;
  bool bad = false;
  uint32_t u1() {
    if (pos >= b.size() * 8) { bad = true; return 0; }
    uint32_t v = (b[pos >> 3] >> (7 - (pos & 7))) & 1;
    pos++;
    return v;
  }
  uint32_t u(int n) {
    uint32_t v = 0;
    for (int i = 0; i < n; i++) v = (v << 1) | u1();
    return v;
  }
  uint32_t ue() {
    int lz = 0;
    while (!bad && u1() == 0)
      if (++lz > 31) { bad = true; return 0; }
    return (uint32_t)(((uint64_t)1 << lz) - 1 + u(lz));
  }
  void se() { ue(); }
};

void skip_hrd(Bits& r) {
  uint32_t cpb = r.ue() + 1;
  r.u(8);
  for (uint32_t i = 0; i < cpb && !r.bad; i++) { r.ue(); r.ue(); r.u1(); }
  r.u(20);
}
}  // namespace

// H.264 7.3.2.1.1 and E.1.1, as far as bitstream_restriction.
SpsReorder sps_reorder(const uint8_t* p, size_t n) {
  SpsReorder out;
  Bits r;
  int zeros = 0;
  for (size_t i = 0; i < n; i++) {
    if (zeros >= 2 && p[i] == 3) { zeros = 0; continue; }
    r.b.push_back(p[i]);
    zeros = p[i] == 0 ? zeros + 1 : 0;
  }
  if (r.b.size() < 4 || (r.b[0] & 0x1f) != 7) return out;
  r.u(8);
  uint32_t profile = r.u(8);
  r.u(16);  // constraint flags, level_idc
  r.ue();   // seq_parameter_set_id
  static const uint32_t high[] = {100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135};
  if (std::find(std::begin(high), std::end(high), profile) != std::end(high)) {
    uint32_t chroma = r.ue();
    if (chroma == 3) r.u1();
    r.ue(); r.ue(); r.u1();
    if (r.u1()) {
      for (int i = 0; i < (chroma != 3 ? 8 : 12) && !r.bad; i++) {
        if (!r.u1()) continue;
        int last = 8, next = 8;
        for (int j = 0; j < (i < 6 ? 16 : 64) && !r.bad; j++) {
          if (next != 0) {
            uint32_t k = r.ue();
            int d = (k & 1) ? (int)((k + 1) / 2) : -(int)(k / 2);
            next = ((last + d) % 256 + 256) % 256;
          }
          if (next != 0) last = next;
        }
      }
    }
  }
  r.ue();  // log2_max_frame_num_minus4
  uint32_t poc = r.ue();
  if (poc == 0) r.ue();
  else if (poc == 1) {
    r.u1(); r.se(); r.se();
    uint32_t c = r.ue();
    for (uint32_t i = 0; i < c && !r.bad; i++) r.se();
  }
  out.max_num_ref_frames = r.ue();
  r.u1();
  r.ue(); r.ue();
  if (!r.u1()) r.u1();  // frame_mbs_only_flag, mb_adaptive_frame_field_flag
  r.u1();
  if (r.u1()) { r.ue(); r.ue(); r.ue(); r.ue(); }
  if (r.u1()) {  // vui_parameters_present_flag
    if (r.u1() && r.u(8) == 255) r.u(32);
    if (r.u1()) r.u1();
    if (r.u1()) { r.u(4); if (r.u1()) r.u(24); }
    if (r.u1()) { r.ue(); r.ue(); }
    if (r.u1()) { r.u(32); r.u(32); r.u1(); }
    uint32_t nal_hrd = r.u1();
    if (nal_hrd) skip_hrd(r);
    uint32_t vcl_hrd = r.u1();
    if (vcl_hrd) skip_hrd(r);
    if (nal_hrd || vcl_hrd) r.u1();
    r.u1();  // pic_struct_present_flag
    if (r.u1()) {
      out.restriction = true;
      r.u1(); r.ue(); r.ue(); r.ue(); r.ue();
      out.max_num_reorder_frames = r.ue();
      out.max_dec_frame_buffering = r.ue();
    }
  }
  // what follows must be rbsp_trailing_bits: a 1, then zeros
  if (r.bad || r.u1() != 1) return SpsReorder();
  while (r.pos < r.b.size() * 8)
    if (r.u1()) return SpsReorder();
  out.parsed = true;
  return out;
}

std::string avc_codec_string(const uint8_t* sps, size_t n) {
  if (n < 4) return "avc1.42E01E";
  char b[32];
  snprintf(b, sizeof b, "avc1.%02X%02X%02X", sps[1], sps[2], sps[3]);
  return b;
}

bool H264Encoder::min_size(void* cu_ctx, int* w, int* h, std::string* err) {
  static int mw = 0, mh = 0;
  if (mw) { *w = mw; *h = mh; return true; }
  if (!load(err)) return false;
  NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS op;
  memset(&op, 0, sizeof op);
  op.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
  op.device = cu_ctx;
  op.deviceType = NV_ENC_DEVICE_TYPE_CUDA;
  op.apiVersion = NVENCAPI_VERSION;
  void* e = nullptr;
  NVENCSTATUS s = g_fl.nvEncOpenEncodeSessionEx(&op, &e);
  if (s != NV_ENC_SUCCESS) { *err = st("nvEncOpenEncodeSessionEx", s); return false; }
  auto cap = [&](NV_ENC_CAPS c) {
    NV_ENC_CAPS_PARAM cp;
    memset(&cp, 0, sizeof cp);
    cp.version = NV_ENC_CAPS_PARAM_VER;
    cp.capsToQuery = c;
    int v = 0;
    g_fl.nvEncGetEncodeCaps(e, NV_ENC_CODEC_H264_GUID, &cp, &v);
    return v;
  };
  mw = std::max(16, cap(NV_ENC_CAPS_WIDTH_MIN));
  mh = std::max(16, cap(NV_ENC_CAPS_HEIGHT_MIN));
  g_fl.nvEncDestroyEncoder(e);
  *w = mw; *h = mh;
  return true;
}

H264Encoder::~H264Encoder() {
  if (!enc_) return;
  if (bs_) g_fl.nvEncDestroyBitstreamBuffer(enc_, bs_);
  if (reg_) g_fl.nvEncUnregisterResource(enc_, reg_);
  g_fl.nvEncDestroyEncoder(enc_);
}

int quality_from_name(const std::string& n) {
  if (n == "lo") return 0;
  if (n == "std") return 1;
  if (n == "hi") return 2;
  return -1;
}
const char* quality_name(int q) { return q == 0 ? "lo" : q == 2 ? "hi" : "std"; }

bool H264Encoder::open(void* cu_ctx, void* nv12, size_t pitch, int w, int h, int fps, int q, std::string* err) {
  if (!load(err)) return false;
  w_ = w; h_ = h; pitch_ = pitch;
  NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS op;
  memset(&op, 0, sizeof op);
  op.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
  op.device = cu_ctx;
  op.deviceType = NV_ENC_DEVICE_TYPE_CUDA;
  op.apiVersion = NVENCAPI_VERSION;
  NVENCSTATUS s = g_fl.nvEncOpenEncodeSessionEx(&op, &enc_);
  if (s != NV_ENC_SUCCESS) { enc_ = nullptr; *err = st("nvEncOpenEncodeSessionEx", s); return false; }

  NV_ENC_PRESET_CONFIG pc;
  memset(&pc, 0, sizeof pc);
  pc.version = NV_ENC_PRESET_CONFIG_VER;
  pc.presetCfg.version = NV_ENC_CONFIG_VER;
  // Rate per quality, in bits per pixel of the delivered picture at 30 fps:
  //   lo   0.25 (floor 0.15 Mbit/s): many small tiles straight to a browser
  //   std  0.8  (floor 0.6 Mbit/s): each tile is encoded on its own, and coding
  //        error that differs on the two sides of a tile edge shows as a seam,
  //        so tiles get more bits than one image of their size would
  //   hi   4    (floor 2, cap 160 Mbit/s): near-lossless, for an aggregator on
  //        the same host that decodes and re-encodes once; preset P4 keeps the
  //        encoder fast at that rate
  // ENC_BPP overrides std. The VBV holds half a second, so the keyframe every
  // 2 s can be several times a P-frame's size instead of being squeezed into
  // one frame's share (which smeared every keyframe).
  GUID preset = q == 2 ? NV_ENC_PRESET_P4_GUID : NV_ENC_PRESET_P6_GUID;
  s = g_fl.nvEncGetEncodePresetConfigEx(enc_, NV_ENC_CODEC_H264_GUID, preset,
                                        NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY, &pc);
  if (s != NV_ENC_SUCCESS) { *err = st("nvEncGetEncodePresetConfigEx", s); return false; }
  NV_ENC_CONFIG cfg = pc.presetCfg;
  cfg.version = NV_ENC_CONFIG_VER;
  cfg.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
  cfg.gopLength = NVENC_INFINITE_GOPLENGTH;
  cfg.frameIntervalP = 1;  // no B-frames
  const char* bpp_env = getenv("ENC_BPP");
  double bpp = q == 0 ? 0.25 : q == 2 ? 4.0 : (bpp_env && atof(bpp_env) > 0 ? atof(bpp_env) : 0.8);
  double floor_bps = q == 0 ? 150000.0 : q == 2 ? 2000000.0 : 600000.0;
  double cap_bps = q == 2 ? 160000000.0 : 20000000.0;
  uint32_t rate = (uint32_t)std::max(floor_bps, std::min(cap_bps, (double)w * h * fps * bpp));
  rate_ = rate;
  cfg.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
  cfg.rcParams.averageBitRate = rate;
  cfg.rcParams.maxBitRate = rate;
  cfg.rcParams.vbvBufferSize = rate / 2;
  cfg.rcParams.vbvInitialDelay = cfg.rcParams.vbvBufferSize;
  cfg.rcParams.zeroReorderDelay = 1;
  NV_ENC_CONFIG_H264& hc = cfg.encodeCodecConfig.h264Config;
  hc.idrPeriod = NVENC_INFINITE_GOPLENGTH;
  hc.repeatSPSPPS = 1;
  hc.disableSPSPPS = 0;
  hc.outputAUD = 0;
  hc.sliceMode = 0;
  hc.sliceModeData = 0;
  NV_ENC_CONFIG_H264_VUI_PARAMETERS& vui = hc.h264VUIParameters;
  vui.videoSignalTypePresentFlag = 1;
  vui.videoFormat = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
  vui.videoFullRangeFlag = 0;
  vui.colourDescriptionPresentFlag = 1;
  vui.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES_BT709;
  vui.transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
  vui.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS_BT709;
  // With no bitstream_restriction a decoder must assume frames are reordered
  // by up to the level's MaxDpbFrames, and holds that many before outputting
  // one (7 or 8 frames, 234 to 268 ms, in Chrome on macOS). With it NVENC
  // writes max_num_reorder_frames 0 (no B-frames) and max_dec_frame_buffering
  // max_num_ref_frames; open() reads both back.
  vui.bitstreamRestrictionFlag = 1;

  NV_ENC_INITIALIZE_PARAMS ip;
  memset(&ip, 0, sizeof ip);
  ip.version = NV_ENC_INITIALIZE_PARAMS_VER;
  ip.encodeGUID = NV_ENC_CODEC_H264_GUID;
  ip.presetGUID = preset;
  ip.tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
  ip.encodeWidth = w; ip.encodeHeight = h;
  ip.darWidth = w; ip.darHeight = h;
  ip.maxEncodeWidth = w; ip.maxEncodeHeight = h;
  ip.frameRateNum = fps; ip.frameRateDen = 1;
  ip.enablePTD = 1;
  ip.encodeConfig = &cfg;
  s = g_fl.nvEncInitializeEncoder(enc_, &ip);
  if (s != NV_ENC_SUCCESS) { *err = st("nvEncInitializeEncoder", s); return false; }

  NV_ENC_REGISTER_RESOURCE rr;
  memset(&rr, 0, sizeof rr);
  rr.version = NV_ENC_REGISTER_RESOURCE_VER;
  rr.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR;
  rr.width = w; rr.height = h; rr.pitch = (uint32_t)pitch;
  rr.resourceToRegister = nv12;
  rr.bufferFormat = NV_ENC_BUFFER_FORMAT_NV12;
  rr.bufferUsage = NV_ENC_INPUT_IMAGE;
  s = g_fl.nvEncRegisterResource(enc_, &rr);
  if (s != NV_ENC_SUCCESS) { *err = st("nvEncRegisterResource", s); return false; }
  reg_ = rr.registeredResource;

  NV_ENC_CREATE_BITSTREAM_BUFFER bb;
  memset(&bb, 0, sizeof bb);
  bb.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
  s = g_fl.nvEncCreateBitstreamBuffer(enc_, &bb);
  if (s != NV_ENC_SUCCESS) { *err = st("nvEncCreateBitstreamBuffer", s); return false; }
  bs_ = bb.bitstreamBuffer;

  // codec string from the encoder's own SPS
  uint8_t hdr[1024];
  uint32_t hn = 0;
  NV_ENC_SEQUENCE_PARAM_PAYLOAD sp;
  memset(&sp, 0, sizeof sp);
  sp.version = NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER;
  sp.inBufferSize = sizeof hdr;
  sp.spsppsBuffer = hdr;
  sp.outSPSPPSPayloadSize = &hn;
  s = g_fl.nvEncGetSequenceParams(enc_, &sp);
  if (s != NV_ENC_SUCCESS) { *err = st("nvEncGetSequenceParams", s); return false; }
  size_t sl = 0;
  long off = find_sps(hdr, hn, &sl);
  if (off < 0) { *err = "no SPS in the encoder's sequence header"; return false; }
  codec_ = avc_codec_string(hdr + off, sl);
  reorder_ = sps_reorder(hdr + off, sl);
  if (!reorder_.parsed || !reorder_.restriction || reorder_.max_num_reorder_frames != 0)
    fprintf(stderr, "fleet-renderer: encoder %dx%d: the SPS does not say frames are never reordered "
            "(parsed %d, restriction %d, max_num_reorder_frames %u); decoders will hold frames\n",
            w, h, (int)reorder_.parsed, (int)reorder_.restriction, reorder_.max_num_reorder_frames);
  return true;
}

bool H264Encoder::encode(bool force_idr, std::vector<uint8_t>* out, bool* keyframe, std::string* err) {
  NV_ENC_MAP_INPUT_RESOURCE mr;
  memset(&mr, 0, sizeof mr);
  mr.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
  mr.registeredResource = reg_;
  NVENCSTATUS s = g_fl.nvEncMapInputResource(enc_, &mr);
  if (s != NV_ENC_SUCCESS) { *err = st("nvEncMapInputResource", s); return false; }
  NV_ENC_PIC_PARAMS pp;
  memset(&pp, 0, sizeof pp);
  pp.version = NV_ENC_PIC_PARAMS_VER;
  pp.inputBuffer = mr.mappedResource;
  pp.bufferFmt = mr.mappedBufferFmt;
  pp.inputWidth = w_; pp.inputHeight = h_; pp.inputPitch = (uint32_t)pitch_;
  pp.outputBitstream = bs_;
  pp.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
  pp.inputTimeStamp = n_++;
  if (force_idr) pp.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
  s = g_fl.nvEncEncodePicture(enc_, &pp);
  if (s != NV_ENC_SUCCESS) {
    g_fl.nvEncUnmapInputResource(enc_, mr.mappedResource);
    *err = st("nvEncEncodePicture", s);
    return false;
  }
  NV_ENC_LOCK_BITSTREAM lb;
  memset(&lb, 0, sizeof lb);
  lb.version = NV_ENC_LOCK_BITSTREAM_VER;
  lb.outputBitstream = bs_;
  s = g_fl.nvEncLockBitstream(enc_, &lb);
  if (s != NV_ENC_SUCCESS) {
    g_fl.nvEncUnmapInputResource(enc_, mr.mappedResource);
    *err = st("nvEncLockBitstream", s);
    return false;
  }
  const uint8_t* p = (const uint8_t*)lb.bitstreamBufferPtr;
  out->assign(p, p + lb.bitstreamSizeInBytes);
  *keyframe = lb.pictureType == NV_ENC_PIC_TYPE_IDR;
  g_fl.nvEncUnlockBitstream(enc_, bs_);
  g_fl.nvEncUnmapInputResource(enc_, mr.mappedResource);
  return true;
}
