#include "nvenc.h"

#include <dlfcn.h>
#include <stdlib.h>

#include <cstdio>
#include <cstring>
#include <mutex>
#include <algorithm>

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

bool H264Encoder::open(void* cu_ctx, void* nv12, size_t pitch, int w, int h, int fps, std::string* err) {
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
  s = g_fl.nvEncGetEncodePresetConfigEx(enc_, NV_ENC_CODEC_H264_GUID, NV_ENC_PRESET_P6_GUID,
                                        NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY, &pc);
  if (s != NV_ENC_SUCCESS) { *err = st("nvEncGetEncodePresetConfigEx", s); return false; }
  NV_ENC_CONFIG cfg = pc.presetCfg;
  cfg.version = NV_ENC_CONFIG_VER;
  cfg.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
  cfg.gopLength = NVENC_INFINITE_GOPLENGTH;
  cfg.frameIntervalP = 1;  // no B-frames
  // About 0.8 bits per pixel at 30 fps (ENC_BPP): each tile is encoded on its
  // own, and coding error that differs on the two sides of a tile edge shows as
  // a seam, so tiles get more bits than one image of their size would. The VBV holds half
  // a second, so the keyframe every 2 s can be several times a P-frame's size
  // instead of being squeezed into one frame's share (which smeared every keyframe).
  const char* bpp_env = getenv("ENC_BPP");
  double bpp = bpp_env && atof(bpp_env) > 0 ? atof(bpp_env) : 0.8;
  uint32_t rate = (uint32_t)std::max(600000.0, std::min(20000000.0, (double)w * h * fps * bpp));
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

  NV_ENC_INITIALIZE_PARAMS ip;
  memset(&ip, 0, sizeof ip);
  ip.version = NV_ENC_INITIALIZE_PARAMS_VER;
  ip.encodeGUID = NV_ENC_CODEC_H264_GUID;
  ip.presetGUID = NV_ENC_PRESET_P6_GUID;
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
