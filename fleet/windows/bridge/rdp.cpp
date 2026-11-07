#include "rdp.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <fstream>
#include <iterator>
#include <thread>

#include <freerdp/addin.h>
#include <freerdp/channels/ainput.h>
#include <freerdp/channels/channels.h>
#include <freerdp/channels/drdynvc.h>
#include <freerdp/channels/rdpgfx.h>
#include <freerdp/client.h>
#include <freerdp/client/channels.h>
#include <freerdp/client/cmdline.h>
#include <freerdp/client/rdpgfx.h>
#include <freerdp/codec/color.h>
#include <freerdp/freerdp.h>
#include <freerdp/gdi/gdi.h>
#include <freerdp/gdi/gfx.h>
#include <freerdp/input.h>
#include <freerdp/scancode.h>
#include <freerdp/settings.h>
#include <freerdp/transport_io.h>
#include <winpr/synch.h>
#include <winpr/wlog.h>

// logic.h numbers scancodes without FreeRDP's headers; they must be FreeRDP's.
static_assert(SC_ESCAPE == RDP_SCANCODE_ESCAPE, "scancode");
static_assert(SC_BACKSPACE == RDP_SCANCODE_BACKSPACE, "scancode");
static_assert(SC_TAB == RDP_SCANCODE_TAB, "scancode");
static_assert(SC_RETURN == RDP_SCANCODE_RETURN, "scancode");
static_assert(SC_LCONTROL == RDP_SCANCODE_LCONTROL, "scancode");
static_assert(SC_LSHIFT == RDP_SCANCODE_LSHIFT, "scancode");
static_assert(SC_LMENU == RDP_SCANCODE_LMENU, "scancode");
static_assert(SC_SPACE == RDP_SCANCODE_SPACE, "scancode");
static_assert(SC_F4 == RDP_SCANCODE_F4, "scancode");
static_assert(SC_KEY_E == RDP_SCANCODE_KEY_E, "scancode");
static_assert(SC_KEY_R == RDP_SCANCODE_KEY_R, "scancode");
static_assert(SC_LWIN == RDP_SCANCODE_LWIN, "scancode");
static_assert(0x10 == RDP_SCANCODE_KEY_Q && 0x1E == RDP_SCANCODE_KEY_A && 0x2C == RDP_SCANCODE_KEY_Z, "scancode");
static_assert(0x02 == RDP_SCANCODE_KEY_1 && 0x0B == RDP_SCANCODE_KEY_0 && 0x0C == RDP_SCANCODE_OEM_MINUS, "scancode");
static_assert(0x33 == RDP_SCANCODE_OEM_COMMA && 0x34 == RDP_SCANCODE_OEM_PERIOD, "scancode");

namespace {
struct BridgeContext {
  rdpClientContext common;
  RdpLink* link;
};
RdpLink* link_of(rdpContext* c) { return ((BridgeContext*)c)->link; }

// The rdpgfx EndFrame runs the GDI's, which blits each surface between a BeginPaint and an
// EndPaint; the link sends one frame for the whole of it.
thread_local bool t_in_gfx_frame = false;
thread_local bool t_gfx_dirty = false;

bool set_bool(rdpSettings* s, const char* name, bool v) {
  SSIZE_T k = freerdp_settings_get_key_for_name((std::string("FreeRDP_") + name).c_str());
  return k >= 0 && freerdp_settings_get_type_for_key(k) == RDP_SETTINGS_TYPE_BOOL &&
         freerdp_settings_set_bool(s, (FreeRDP_Settings_Keys_Bool)k, v ? TRUE : FALSE);
}

// What the connection is set up with, for settings_violation (logic.h).
RdpSettingsView settings_view(const rdpSettings* s) {
  RdpSettingsView v;
  auto read = [&](const char* const* names, int n) {
    for (int i = 0; i < n; i++) {
      SSIZE_T k = freerdp_settings_get_key_for_name((std::string("FreeRDP_") + names[i]).c_str());
      if (k < 0 || freerdp_settings_get_type_for_key(k) != RDP_SETTINGS_TYPE_BOOL) continue;
      v.bools.push_back({names[i], freerdp_settings_get_bool(s, (FreeRDP_Settings_Keys_Bool)k) != FALSE});
    }
  };
  read(kForbiddenSettings, kForbiddenSettingCount);
  read(kOffSettings, kOffSettingCount);
  read(kRequiredSettings, kRequiredSettingCount);
  auto channels = [&](FreeRDP_Settings_Keys_UInt32 count, FreeRDP_Settings_Keys_Pointer array, std::vector<std::string>* out) {
    for (UINT32 i = 0; i < freerdp_settings_get_uint32(s, count); i++) {
      const ADDIN_ARGV* a = (const ADDIN_ARGV*)freerdp_settings_get_pointer_array(s, array, i);
      out->push_back(a && a->argc > 0 && a->argv[0] ? a->argv[0] : "(unnamed)");
    }
  };
  channels(FreeRDP_StaticChannelCount, FreeRDP_StaticChannelArray, &v.static_channels);
  channels(FreeRDP_DynamicChannelCount, FreeRDP_DynamicChannelArray, &v.dynamic_channels);
  v.devices = freerdp_settings_get_uint32(s, FreeRDP_DeviceCount);
  v.proxy_ignored = freerdp_settings_get_uint32(s, FreeRDP_ProxyType) == PROXY_TYPE_IGNORE;
  return v;
}

std::string read_password(const std::string& path, std::string* err) {
  if (path.empty()) {
    *err = "RDP_PASSWORD_FILE is not set";
    return "";
  }
  std::ifstream f(path, std::ios::binary);
  if (!f) {
    *err = "RDP_PASSWORD_FILE cannot be read";
    return "";
  }
  std::string p((std::istreambuf_iterator<char>(f)), std::istreambuf_iterator<char>());
  while (!p.empty() && (p.back() == '\n' || p.back() == '\r')) p.pop_back();
  if (p.empty()) *err = "RDP_PASSWORD_FILE is empty";
  return p;
}

void wipe(std::string* s) {
  volatile char* p = s->empty() ? nullptr : &(*s)[0];
  for (size_t i = 0; p && i < s->size(); i++) p[i] = 0;
  s->clear();
}
}  // namespace

struct RdpLink::Hooks {
  static BOOL begin_paint(rdpContext* c) {
    rdpGdi* gdi = c->gdi;
    if (gdi && gdi->primary && gdi->primary->hdc && gdi->primary->hdc->hwnd && gdi->primary->hdc->hwnd->invalid)
      gdi->primary->hdc->hwnd->invalid->null = TRUE;
    return TRUE;
  }
  static void emit(rdpContext* c, bool gfx) {
    rdpGdi* gdi = c->gdi;
    if (!gdi || !gdi->primary_buffer) return;
    link_of(c)->sink_->on_frame(gdi->primary_buffer, (int)gdi->width, (int)gdi->height, (size_t)gdi->stride, unix_ms_now(),
                                gfx);
  }
  static BOOL end_paint(rdpContext* c) {
    rdpGdi* gdi = c->gdi;
    if (!gdi || !gdi->primary || !gdi->primary->hdc || !gdi->primary->hdc->hwnd) return TRUE;
    if (gdi->primary->hdc->hwnd->invalid->null) return TRUE;  // nothing drawn
    if (t_in_gfx_frame) {
      t_gfx_dirty = true;
      return TRUE;
    }
    emit(c, false);
    return TRUE;
  }
  static BOOL desktop_resize(rdpContext* c) {
    return gdi_resize(c->gdi, freerdp_settings_get_uint32(c->settings, FreeRDP_DesktopWidth),
                      freerdp_settings_get_uint32(c->settings, FreeRDP_DesktopHeight));
  }
  static UINT gfx_end_frame(RdpgfxClientContext* gfx, const RDPGFX_END_FRAME_PDU* end) {
    rdpGdi* gdi = (rdpGdi*)gfx->custom;
    rdpContext* c = gdi->context;
    auto orig = (pcRdpgfxEndFrame)link_of(c)->gfx_end_frame_;
    t_in_gfx_frame = true;
    t_gfx_dirty = false;
    UINT rc = orig(gfx, end);
    t_in_gfx_frame = false;
    if (t_gfx_dirty) emit(c, true);
    return rc;
  }
  static UINT gfx_surface_command(RdpgfxClientContext* gfx, const RDPGFX_SURFACE_COMMAND* cmd) {
    rdpGdi* gdi = (rdpGdi*)gfx->custom;
    RdpLink* l = link_of(gdi->context);
    l->sink_->on_gfx_command(cmd->codecId);
    return ((pcRdpgfxSurfaceCommand)l->gfx_surface_command_)(gfx, cmd);
  }
  static void channel_connected(void* ctx, const ChannelConnectedEventArgs* e) {
    freerdp_client_OnChannelConnectedEventHandler(ctx, e);
    if (strcmp(e->name, RDPGFX_DVC_CHANNEL_NAME) == 0) {
      // the GDI's handlers are in place now: wrap its EndFrame
      auto* gfx = (RdpgfxClientContext*)e->pInterface;
      RdpLink* l = link_of((rdpContext*)ctx);
      l->gfx_end_frame_ = (void*)gfx->EndFrame;
      gfx->EndFrame = gfx_end_frame;
      l->gfx_surface_command_ = (void*)gfx->SurfaceCommand;
      gfx->SurfaceCommand = gfx_surface_command;
    }
  }
  static void channel_disconnected(void* ctx, const ChannelDisconnectedEventArgs* e) {
    if (strcmp(e->name, RDPGFX_DVC_CHANNEL_NAME) == 0) {
      auto* gfx = (RdpgfxClientContext*)e->pInterface;
      RdpLink* l = link_of((rdpContext*)ctx);
      if (gfx->EndFrame == gfx_end_frame) gfx->EndFrame = (pcRdpgfxEndFrame)l->gfx_end_frame_;
      if (gfx->SurfaceCommand == gfx_surface_command) gfx->SurfaceCommand = (pcRdpgfxSurfaceCommand)l->gfx_surface_command_;
    }
    freerdp_client_OnChannelDisconnectedEventHandler(ctx, e);
  }
  static int tcp_connect(rdpContext* c, rdpSettings* s, const char* host, int port, DWORD timeout) {
    RdpLink* l = link_of(c);
    int fd = ((pTCPConnect)l->tcp_connect_)(c, s, host, port, timeout);
    if (fd >= 0) l->sink_->on_tcp(unix_ms_now());
    return fd;
  }
  static BOOL pre_connect(freerdp* instance) {
    rdpContext* c = instance->context;
    if (!freerdp_settings_set_uint32(c->settings, FreeRDP_OsMajorType, OSMAJORTYPE_UNIX) ||
        !freerdp_settings_set_uint32(c->settings, FreeRDP_OsMinorType, OSMINORTYPE_PSEUDO_XSERVER))
      return FALSE;
    PubSub_SubscribeChannelConnected(c->pubSub, channel_connected);
    PubSub_SubscribeChannelDisconnected(c->pubSub, channel_disconnected);
    return TRUE;
  }
  // The link loads its channels itself, in place of FreeRDP's client loader (client/common),
  // which maps settings to channels and adds device redirection and a fake audio channel for
  // network autodetect and the heartbeat: the graphics pipeline
  // and input, over the dynamic channel transport, and nothing else. The settings check runs
  // over the result before the transport loads, and refuses the connection.
  static bool load_static(rdpContext* c, const char* name, void* data) {
    PVIRTUALCHANNELENTRY e =
        freerdp_load_channel_addin_entry(name, nullptr, nullptr, FREERDP_ADDIN_CHANNEL_STATIC | FREERDP_ADDIN_CHANNEL_ENTRYEX);
    if (e) return freerdp_channels_client_load_ex(c->channels, c->settings, (PVIRTUALCHANNELENTRYEX)(void*)e, data) == 0;
    e = freerdp_load_channel_addin_entry(name, nullptr, nullptr, FREERDP_ADDIN_CHANNEL_STATIC);
    return e && freerdp_channels_client_load(c->channels, c->settings, e, data) == 0;
  }
  static BOOL load_channels(freerdp* instance) {
    rdpSettings* s = instance->context->settings;
    for (const char* n : {RDPGFX_CHANNEL_NAME, AINPUT_CHANNEL_NAME}) {
      const char* p[] = {n};
      if (!freerdp_dynamic_channel_collection_find(s, n) && !freerdp_client_add_dynamic_channel(s, 1, p)) return FALSE;
    }
    std::string v = settings_violation(settings_view(instance->context->settings));
    if (!v.empty()) {
      link_of(instance->context)->fatal_ = "the connection's settings are refused: " + v;
      return FALSE;
    }
    return freerdp_settings_set_bool(s, FreeRDP_SupportDynamicChannels, TRUE) &&
           load_static(instance->context, DRDYNVC_SVC_CHANNEL_NAME, s);
  }
  static BOOL post_connect(freerdp* instance) {
    rdpContext* c = instance->context;
    if (!gdi_init(instance, PIXEL_FORMAT_BGRX32)) return FALSE;
    c->update->BeginPaint = begin_paint;
    c->update->EndPaint = end_paint;
    c->update->DesktopResize = desktop_resize;
    link_of(c)->sink_->on_rdp(unix_ms_now());
    return TRUE;
  }
  static void post_disconnect(freerdp* instance) {
    rdpContext* c = instance->context;
    PubSub_UnsubscribeChannelConnected(c->pubSub, channel_connected);
    PubSub_UnsubscribeChannelDisconnected(c->pubSub, channel_disconnected);
    gdi_free(instance);
  }
  // Nobody is asked anything: the password comes from its file, and the certificate (the
  // gold image's own, self-signed, on a connection that never leaves the host) is accepted
  // for this connection only and never stored. FreeRDP asks before TLS even with credentials
  // set (utils_authenticate); the answer keeps them as configured, and anything else asked
  // (a gateway, a smart card PIN) is refused.
  static BOOL authenticate(freerdp*, char** user, char** password, char**, rdp_auth_reason reason) {
    return (reason == AUTH_TLS || reason == AUTH_RDP) && user && *user && **user && password && *password && **password;
  }
  static DWORD verify_certificate(freerdp*, const char*, UINT16, const char*, const char*, const char*, const char*, DWORD) {
    return 2;
  }
  static DWORD verify_changed_certificate(freerdp*, const char*, UINT16, const char*, const char*, const char*, const char*,
                                          const char*, const char*, const char*, DWORD) {
    return 2;
  }
  static BOOL client_new(freerdp* instance, rdpContext*) {
    instance->PreConnect = pre_connect;
    instance->PostConnect = post_connect;
    instance->PostDisconnect = post_disconnect;
    instance->LoadChannels = load_channels;
    instance->AuthenticateEx = authenticate;
    instance->VerifyCertificateEx = verify_certificate;
    instance->VerifyChangedCertificateEx = verify_changed_certificate;
    return TRUE;
  }
};

bool RdpLink::built_with_h264() {
  const char* c = freerdp_get_build_config();
  return c && strstr(c, "WITH_GFX_H264=ON");
}

namespace {
// The connection's settings. Every setting the channel check reads is set here, so a FreeRDP
// whose defaults change cannot turn one on.
bool configure(rdpSettings* s, const RdpConfig& cfg, const std::string& password) {
  bool ok = freerdp_settings_set_string(s, FreeRDP_ServerHostname, cfg.socket.c_str()) &&
            freerdp_settings_set_uint32(s, FreeRDP_ServerPort, 3389) &&
            freerdp_settings_set_string(s, FreeRDP_Username, cfg.user.c_str()) &&
            freerdp_settings_set_string(s, FreeRDP_Password, password.c_str()) &&
            freerdp_settings_set_string(s, FreeRDP_Domain, "") &&
            freerdp_settings_set_uint32(s, FreeRDP_DesktopWidth, (UINT32)cfg.width) &&
            freerdp_settings_set_uint32(s, FreeRDP_DesktopHeight, (UINT32)cfg.height) &&
            freerdp_settings_set_uint32(s, FreeRDP_ColorDepth, 32) &&
            freerdp_settings_set_uint32(s, FreeRDP_KeyboardLayout, 0x409) &&
            // the connection never leaves the host, whatever https_proxy says
            freerdp_settings_set_uint32(s, FreeRDP_ProxyType, PROXY_TYPE_IGNORE) &&
            freerdp_settings_set_bool(s, FreeRDP_IgnoreCertificate, TRUE) &&
            freerdp_settings_set_bool(s, FreeRDP_SoftwareGdi, TRUE) &&
            freerdp_settings_set_bool(s, FreeRDP_SupportDynamicChannels, TRUE) &&
            freerdp_settings_set_bool(s, FreeRDP_SupportDisplayControl, FALSE) &&
            freerdp_settings_set_bool(s, FreeRDP_GfxProgressive, TRUE) &&
            freerdp_settings_set_bool(s, FreeRDP_GfxThinClient, FALSE) &&
            freerdp_set_connection_type(s, CONNECTION_TYPE_AUTODETECT);
  // the lists last: freerdp_set_connection_type sets some of them
  for (int i = 0; ok && i < kForbiddenSettingCount; i++) ok = set_bool(s, kForbiddenSettings[i], false);
  for (int i = 0; ok && i < kOffSettingCount; i++) ok = set_bool(s, kOffSettings[i], false);
  for (int i = 0; ok && i < kRequiredSettingCount; i++) ok = set_bool(s, kRequiredSettings[i], true);
  return ok;
}
}  // namespace

std::string RdpLink::attempt(bool* reached, bool* fatal) {
  *reached = false;
  *fatal = false;
  fatal_.clear();
  gfx_end_frame_ = nullptr;
  gfx_surface_command_ = nullptr;
  std::string err;
  std::string password = read_password(cfg_.password_file, &err);
  if (!err.empty()) return err;

  RDP_CLIENT_ENTRY_POINTS ep;
  memset(&ep, 0, sizeof ep);
  ep.Version = RDP_CLIENT_INTERFACE_VERSION;
  ep.Size = sizeof(RDP_CLIENT_ENTRY_POINTS_V1);
  ep.ContextSize = sizeof(BridgeContext);
  ep.ClientNew = Hooks::client_new;
  rdpContext* ctx = freerdp_client_context_new(&ep);
  if (!ctx) {
    wipe(&password);
    return "freerdp_client_context_new failed";
  }
  ((BridgeContext*)ctx)->link = this;
  bool ok = configure(ctx->settings, cfg_, password);
  wipe(&password);
  // The checked settings as configured, before FreeRDP adds its channels (checked again then).
  std::string v = ok ? settings_violation(settings_view(ctx->settings)) : "";
  if (!ok || !v.empty()) {
    freerdp_client_context_free(ctx);
    *fatal = true;
    return ok ? "the connection's settings are refused: " + v : "a FreeRDP setting could not be set";
  }
  // Time the socket connect: wrap FreeRDP's own.
  const rdpTransportIo* io = freerdp_get_io_callbacks(ctx);
  rdpTransportIo hooked = *io;
  tcp_connect_ = (void*)hooked.TCPConnect;
  hooked.TCPConnect = Hooks::tcp_connect;
  if (!freerdp_set_io_callbacks(ctx, &hooked) || freerdp_client_start(ctx) != 0) {
    freerdp_client_context_free(ctx);
    return "FreeRDP could not be set up";
  }
  sink_->on_attempt(unix_ms_now());
  freerdp* instance = ctx->instance;
  if (!freerdp_connect(instance)) {
    UINT32 e = freerdp_get_last_error(ctx);
    if (!fatal_.empty()) {
      err = fatal_;
      *fatal = true;
    } else {
      const char* s = freerdp_get_last_error_string(e);
      char b[64];
      snprintf(b, sizeof b, " (0x%08X)", e);
      err = std::string("connect: ") + (s ? s : "failed") + b;
    }
    freerdp_disconnect(instance);
    freerdp_client_stop(ctx);
    freerdp_client_context_free(ctx);
    return err;
  }
  *reached = true;
  connected_ = true;
  while (!stop_) {
    HANDLE handles[MAXIMUM_WAIT_OBJECTS];
    DWORD n = freerdp_get_event_handles(ctx, handles, MAXIMUM_WAIT_OBJECTS - 1);
    if (n == 0) {
      err = "freerdp_get_event_handles failed";
      break;
    }
    handles[n++] = (HANDLE)wake_;
    DWORD w = WaitForMultipleObjects(n, handles, FALSE, 100);
    if (w == WAIT_FAILED) {
      err = "WaitForMultipleObjects failed";
      break;
    }
    ResetEvent((HANDLE)wake_);
    if (!freerdp_check_event_handles(ctx)) {
      UINT32 e = freerdp_get_last_error(ctx);
      const char* s = freerdp_get_last_error_string(e);
      err = std::string("connection: ") + (s && e ? s : "ended");
      break;
    }
    if (freerdp_shall_disconnect_context(ctx)) {
      err = "the server ended the connection";
      break;
    }
    drain_keys(ctx);
    sink_->on_tick(unix_ms_now());
  }
  connected_ = false;
  fail_keys("the connection ended");
  freerdp_disconnect(instance);
  freerdp_client_stop(ctx);
  freerdp_client_context_free(ctx);
  return err;
}

void RdpLink::drain_keys(void* context) {
  rdpContext* ctx = (rdpContext*)context;
  std::deque<KeyEvent> batch;
  {
    std::lock_guard<std::mutex> l(mu_);
    batch.swap(keys_);
  }
  for (const KeyEvent& k : batch) freerdp_input_send_keyboard_event_ex(ctx->input, k.down ? TRUE : FALSE, FALSE, k.scancode);
  if (batch.empty()) return;
  std::lock_guard<std::mutex> l(mu_);
  sent_ += batch.size();
  cv_.notify_all();
}

void RdpLink::fail_keys(const std::string&) {
  std::lock_guard<std::mutex> l(mu_);
  keys_.clear();
  sent_ = queued_;
  generation_++;
  cv_.notify_all();
}

bool RdpLink::send_keys(const std::vector<KeyEvent>& ev, std::string* err) {
  std::unique_lock<std::mutex> l(mu_);
  if (!connected_) {
    *err = "not connected to the sandbox";
    return false;
  }
  uint64_t gen = generation_;
  for (const KeyEvent& k : ev) keys_.push_back(k);
  queued_ += ev.size();
  uint64_t target = queued_;
  if (wake_) SetEvent((HANDLE)wake_);
  bool done = cv_.wait_for(l, std::chrono::seconds(5), [&] { return sent_ >= target || generation_ != gen; });
  if (generation_ != gen) {
    *err = "the connection ended before the keys were sent";
    return false;
  }
  if (!done) {
    *err = "the keys were not sent within 5 s";
    return false;
  }
  return true;
}

void RdpLink::stop() {
  stop_ = true;
  std::lock_guard<std::mutex> l(mu_);
  if (wake_) SetEvent((HANDLE)wake_);
  cv_.notify_all();
}

void RdpLink::run() {
  // FreeRDP's warnings repeat on every attempt while the sandbox restores; WLOG_LEVEL (winpr's
  // own variable) still sets any level.
  if (!getenv("WLOG_LEVEL")) WLog_SetLogLevel(WLog_GetRoot(), WLOG_ERROR);
  wake_ = CreateEvent(nullptr, TRUE, FALSE, nullptr);
  Backoff backoff;
  while (!stop_) {
    bool reached = false, fatal = false;
    std::string err = attempt(&reached, &fatal);
    if (reached) backoff.reset();
    if (stop_) break;
    sink_->on_down(err, fatal, reached);
    if (fatal) break;
    int ms = backoff.next_ms();
    std::unique_lock<std::mutex> l(mu_);
    double until = mono_ms_now() + ms;
    // the sink keeps its clock (the settle check) moving while nothing is connected
    while (!stop_ && mono_ms_now() < until) {
      cv_.wait_for(l, std::chrono::milliseconds(std::min(100, ms)));
      l.unlock();
      sink_->on_tick(unix_ms_now());
      l.lock();
    }
  }
  std::lock_guard<std::mutex> l(mu_);
  CloseHandle((HANDLE)wake_);
  wake_ = nullptr;
}
