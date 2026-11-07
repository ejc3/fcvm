#include "x264enc.h"

#include <stdio.h>
#include <string.h>

#include <x264.h>

#include "logic.h"

X264Encoder::~X264Encoder() {
  if (enc_) x264_encoder_close(enc_);
}

bool X264Encoder::open(int w, int h, int fps, int q, std::string* err) {
  if (enc_) { x264_encoder_close(enc_); enc_ = nullptr; }
  if (w <= 0 || h <= 0 || (w & 1) || (h & 1)) {
    *err = "x264: width and height must be even and positive";
    return false;
  }
  x264_param_t p;
  if (x264_param_default_preset(&p, "ultrafast", "zerolatency") < 0) {
    *err = "x264: preset ultrafast/zerolatency refused";
    return false;
  }
  p.i_width = w;
  p.i_height = h;
  p.i_csp = X264_CSP_I420;
  p.i_fps_num = (uint32_t)fps;
  p.i_fps_den = 1;
  p.i_timebase_num = 1;
  p.i_timebase_den = (uint32_t)fps;
  // one thread, one slice: zerolatency's sliced threads would cut every picture into slices
  p.i_threads = 1;
  p.b_sliced_threads = 0;
  p.i_slice_count = 1;
  p.i_bframe = 0;
  // keyframes only when asked (a new reader, every 2 s, a reader dropped behind); scene cuts
  // would put extra ones in the middle of a busy desktop
  p.i_keyint_max = X264_KEYINT_MAX_INFINITE;
  p.i_scenecut_threshold = 0;
  p.b_intra_refresh = 0;
  p.b_annexb = 1;
  p.b_repeat_headers = 1;
  p.b_aud = 0;
  // the renderer's VUI: BT.709, limited range
  p.vui.b_fullrange = 0;
  p.vui.i_colorprim = 1;
  p.vui.i_transfer = 1;
  p.vui.i_colmatrix = 1;
  // The renderer's rate per quality is the cap; under it, constant quality, so a still desktop
  // costs almost nothing and a moving one is held to the rate. The VBV holds half a second, as
  // the renderer's does.
  rate_ = quality_rate_bps(q, w, h, fps);
  p.rc.i_rc_method = X264_RC_CRF;
  p.rc.f_rf_constant = quality_crf(q);
  p.rc.i_vbv_max_bitrate = (int)(rate_ / 1000);
  p.rc.i_vbv_buffer_size = (int)(rate_ / 2000);
  p.i_log_level = X264_LOG_WARNING;
  // main: what the page's decoder takes everywhere; ultrafast's tools fit inside it, and x264
  // signals the lowest profile the stream needs
  if (x264_param_apply_profile(&p, "main") < 0) {
    *err = "x264: profile main refused";
    return false;
  }
  enc_ = x264_encoder_open(&p);
  if (!enc_) {
    *err = "x264_encoder_open failed";
    return false;
  }
  w_ = w;
  h_ = h;
  pts_ = 0;
  x264_nal_t* nals = nullptr;
  int n = 0;
  if (x264_encoder_headers(enc_, &nals, &n) < 0) {
    *err = "x264_encoder_headers failed";
    return false;
  }
  std::vector<uint8_t> hdr;
  for (int i = 0; i < n; i++) hdr.insert(hdr.end(), nals[i].p_payload, nals[i].p_payload + nals[i].i_payload);
  size_t len = 0;
  long at = find_sps(hdr.data(), hdr.size(), &len);
  if (at < 0) {
    *err = "x264 wrote no SPS";
    return false;
  }
  codec_ = avc_codec_string(hdr.data() + at, len);
  reorder_ = sps_reorder(hdr.data() + at, len);
  // Without it a decoder holds the level's MaxDpbFrames before showing a frame (renderer
  // PROTOCOL.md, "GET /frames").
  if (!reorder_.parsed || !reorder_.restriction || reorder_.max_num_reorder_frames != 0) {
    char b[160];
    snprintf(b, sizeof b, "x264's SPS has no bitstream_restriction with no reordering (parsed %d, restriction %d, reorder %u)",
             reorder_.parsed, reorder_.restriction, reorder_.max_num_reorder_frames);
    *err = b;
    return false;
  }
  return true;
}

bool X264Encoder::encode(const uint8_t* y, const uint8_t* u, const uint8_t* v, bool force_idr, std::vector<uint8_t>* out,
                         bool* keyframe, std::string* err) {
  if (!enc_) {
    *err = "encoder not open";
    return false;
  }
  x264_picture_t in, outp;
  x264_picture_init(&in);
  in.img.i_csp = X264_CSP_I420;
  in.img.i_plane = 3;
  in.img.plane[0] = const_cast<uint8_t*>(y);
  in.img.plane[1] = const_cast<uint8_t*>(u);
  in.img.plane[2] = const_cast<uint8_t*>(v);
  in.img.i_stride[0] = w_;
  in.img.i_stride[1] = w_ / 2;
  in.img.i_stride[2] = w_ / 2;
  in.i_pts = pts_++;
  in.i_type = force_idr ? X264_TYPE_IDR : X264_TYPE_AUTO;
  x264_nal_t* nals = nullptr;
  int n = 0;
  int bytes = x264_encoder_encode(enc_, &nals, &n, &in, &outp);
  if (bytes < 0) {
    *err = "x264_encoder_encode failed";
    return false;
  }
  out->clear();
  // zerolatency has no lookahead and no B-frames: every picture in is a picture out
  if (bytes == 0) {
    *err = "x264 returned no picture";
    return false;
  }
  for (int i = 0; i < n; i++) out->insert(out->end(), nals[i].p_payload, nals[i].p_payload + nals[i].i_payload);
  *keyframe = outp.b_keyframe && outp.i_type == X264_TYPE_IDR;
  return true;
}
