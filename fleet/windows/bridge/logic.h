// The Windows bridge's decisions, kept apart from FreeRDP and x264 so they are tested on any
// host (test_host.cpp): the desktop check and its thumbnails, the colour conversion, the
// reconnect schedule, the fixed actions, the channel check, the stream header and /health.
#pragma once
#include <stddef.h>
#include <stdint.h>

#include <deque>
#include <functional>
#include <string>
#include <utility>
#include <vector>

double unix_ms_now();
double mono_ms_now();

// ---- thumbnails and the desktop check

#define THUMB_W 160
#define THUMB_H 90
// A frame is the desktop when its thumbnail is within this RMSE of the reference's (0..1).
#define DESKTOP_RMSE 0.05
// Without a reference: the desktop is the first frame the surface stays at for this long
// (DesktopDetector's settle time), changing by no more than SETTLE_RMSE meanwhile (the
// taskbar clock turning a minute is not a change), and that is not one flat colour.
#define SETTLE_MS 2000.0
#define SETTLE_RMSE 0.01
#define SETTLE_MIN_STDDEV 0.02

// A 160 x 90 picture, RGB, each channel 0..1. Empty when there is none.
struct Thumb {
  std::vector<float> rgb;  // THUMB_W * THUMB_H * 3
  bool empty() const { return rgb.empty(); }
};
// The area average of each 1/160 x 1/90 cell of a BGRX picture (FreeRDP's 32-bit surface:
// blue, green, red, unused, per pixel), or of a packed RGB one.
Thumb thumb_from_bgrx(const uint8_t* bgrx, int w, int h, size_t stride);
Thumb thumb_from_rgb(const uint8_t* rgb, int w, int h);
// Root mean square difference over every channel of every cell, 0..1; 1 when either is empty.
double thumb_rmse(const Thumb& a, const Thumb& b);
// The standard deviation of the cells' luma, 0..0.5: near 0 for a flat picture.
double thumb_stddev(const Thumb& t);

// Image files: binary PPM (P6, maxval 255) or PNG (8-bit; grey, RGB, palette, with or without
// alpha). False with a message.
bool load_image_rgb(const std::string& path, std::vector<uint8_t>* rgb, int* w, int* h, std::string* err);
bool load_thumb(const std::string& path, Thumb* out, std::string* err);
// The thumbnail as a binary PPM, written to a temporary name and renamed into place.
bool write_thumb_ppm(const std::string& path, const Thumb& t, std::string* err);

// Finds the first desktop frame: with a reference, the first frame matching it; without one,
// the first frame the surface settles at (above). Frames arrive in time order.
class DesktopDetector {
 public:
  explicit DesktopDetector(Thumb ref, double settle_ms = SETTLE_MS) : ref_(std::move(ref)), settle_ms_(settle_ms) {}
  // A frame the surface showed from t_ms on.
  void frame(double t_ms, const Thumb& th);
  // The clock moved to now_ms with no new frame (settling needs no frame to finish).
  void tick(double now_ms);
  bool found() const { return found_; }
  double desktop_ms() const { return desktop_ms_; }  // the desktop frame's time
  const Thumb& desktop_thumb() const { return desktop_; }
  const char* by() const { return ref_.empty() ? "settled" : "reference"; }
  bool has_reference() const { return !ref_.empty(); }
  // A connection began or ended: a frame seen before it is no candidate, so a desktop only
  // settles on frames of the connection it is shown on (time spent disconnected counts for
  // nothing). Without a reference only; a found desktop stays found.
  void reset_settle() { have_candidate_ = false; }
  const Thumb& reference() const { return ref_; }

 private:
  Thumb ref_;
  double settle_ms_;
  bool found_ = false;
  double desktop_ms_ = -1;
  Thumb desktop_;
  bool have_candidate_ = false;
  double candidate_ms_ = 0;
  Thumb candidate_;
};

// ---- pictures

// BT.709 limited range, the renderer's and the phone streamer's coefficients. w and h even;
// y has w bytes per row, u and v w/2.
void bgrx_to_i420(const uint8_t* bgrx, size_t stride, int w, int h, uint8_t* y, uint8_t* u, uint8_t* v);
// For GET /still: RGBA with alpha 255.
void bgrx_to_rgba(const uint8_t* bgrx, size_t stride, int w, int h, uint8_t* rgba);

// ---- reconnecting

// 100 ms, doubling to 2 s, then 2 s for ever; reset() after a connection that worked.
class Backoff {
 public:
  int next_ms();
  void reset() { next_ = 100; }

 private:
  int next_ = 100;
};

// ---- the fixed actions (docs/fleet/windows-sandboxes.md, "Actions")
//
// Nothing a caller sends reaches Windows: POST /action names one of these, and the bridge
// sends that action's steps, which are constants here. KeyEvents come only from step_keys().

// RDP scancodes as FreeRDP numbers them (make code, 0x100 for an extended key); rdp.cpp checks
// each against FreeRDP's RDP_SCANCODE_* values.
enum : uint16_t {
  SC_ESCAPE = 0x01,
  SC_BACKSPACE = 0x0E,
  SC_TAB = 0x0F,
  SC_RETURN = 0x1C,
  SC_LCONTROL = 0x1D,
  SC_LSHIFT = 0x2A,
  SC_LMENU = 0x38,  // left Alt
  SC_SPACE = 0x39,
  SC_F4 = 0x3E,
  SC_KEY_E = 0x12,
  SC_KEY_R = 0x13,
  SC_LWIN = 0x15B,
};

struct KeyEvent {
  uint16_t scancode;
  bool down;
  bool operator==(const KeyEvent& o) const { return scancode == o.scancode && down == o.down; }
};

enum StepKind {
  STEP_CHORD,        // press keys[0..n) in order, release in reverse
  STEP_TEXT,         // type text on a US layout
  STEP_WAIT,         // wait ms
  STEP_WAIT_CHANGE,  // wait until the screen changes from what it showed before the last step, at most ms
  STEP_CLOSE_UNTIL_DESKTOP,  // Alt+F4 until the screen is the desktop reference, at most ms times
};
struct Step {
  StepKind kind;
  uint16_t keys[3];
  int nkeys;
  const char* text;
  int ms;
};
struct Action {
  const char* name;
  const Step* steps;
  int nsteps;
};
// The action table: every action, in the order /health and refusals list them.
extern const Action kActions[];
extern const int kActionCount;
const Action* action_find(const std::string& name);
// "start_menu, notepad, ..."
std::string action_names();
// The 400 body's message for a name not in the table.
std::string action_refusal(const std::string& name);
// The action a POST /action body names ({"name": "<action>"}); null with a message otherwise.
const Action* action_from_body(const std::string& body, std::string* err);
// The key events of one chord or text step; false for a character the US map lacks.
bool step_keys(const Step& s, std::vector<KeyEvent>* out);
#define CLOSE_ALL_TRIES 10
// close_all's one key: Alt+F4.
extern const Step kCloseWindowStep;
// How long close_all waits after each Alt+F4 before it looks at the screen again.
#define CLOSE_ALL_WAIT_MS 700
// A screen differs from another (STEP_WAIT_CHANGE) past this thumbnail RMSE.
#define CHANGE_RMSE 0.02

// The key queue between Bridge::play and the link's thread (RdpLink::send_keys and drain_keys,
// which hold its mutex around every call). It holds one batch, a step's balanced presses and
// releases, at a time: a second is refused while the first is queued or being sent. A waiter
// that gives up cancels its batch, which drops it if the link's thread has not taken it yet, so
// keys never go out after their action failed; a batch already taken is being sent and cannot
// be split.
class KeyQueue {
 public:
  enum Status { QUEUED, SENDING, SENT, FAILED };
  // Queues ev under a new ticket; false with a message while another batch is pending.
  bool submit(const std::vector<KeyEvent>& ev, uint64_t* ticket, std::string* err);
  // The link's thread takes the queued batch (SENDING until done()); false when none is queued.
  bool take(std::vector<KeyEvent>* out);
  // The batch taken was sent (ok) or failed with err.
  // released false: a key may still be down, and no batch is taken until fail() (the
  // connection ends; the link reconnects).
  void done(bool ok, const std::string& err, bool released = true);
  // The waiter of ticket gave up: true when its batch was still queued and is now dropped.
  bool cancel(uint64_t ticket);
  // The connection ended: a queued batch is dropped and fails with why.
  void fail(const std::string& why);
  // The state of ticket's batch, and the failure's message. A ticket that is not the latest
  // batch's has failed.
  Status status(uint64_t ticket, std::string* err) const;

 private:
  uint64_t ticket_ = 0;
  Status status_ = SENT;
  std::vector<KeyEvent> batch_;
  std::string err_;
  bool held_ = false;  // a key may be held down until the connection ends
};
// Sends the events in order through send, stopping at the first it refuses; false with which.
// On a failure it then sends, best effort, a release for every key the batch pressed and has
// not released (the failed event's key included), in reverse press order, so a failure does
// not leave Ctrl or Shift down for the next action; *released says whether every one went.
bool send_key_batch(const std::vector<KeyEvent>& ev, const std::function<bool(const KeyEvent&)>& send, std::string* err,
                    bool* released = nullptr);

// ---- the desktop size

// The desktop sizes the bridge takes: RDP_WIDTH and RDP_HEIGHT (main.cpp, both even) and any
// size the server negotiates or resizes to, checked before FreeRDP's GDI allocates the surface.
#define RDP_MIN_SIDE 200
#define RDP_MAX_WIDTH 4096
#define RDP_MAX_HEIGHT 2160
// "" for a size within the bounds, else why it is refused.
std::string desktop_size_violation(uint32_t w, uint32_t h);

// ---- the channel check (docs/fleet/windows-sandboxes.md, "Edition and licensing")

// What the connection was set up with, read from FreeRDP's settings after its channels
// loaded: named boolean settings, and the channels and redirected devices it will offer.
struct RdpSettingsView {
  std::vector<std::pair<std::string, bool>> bools;
  std::vector<std::string> static_channels, dynamic_channels;
  unsigned devices = 0;
  // FreeRDP's ProxyType is PROXY_TYPE_IGNORE: no proxy from the settings or from https_proxy
  // and all_proxy in the environment, which FreeRDP reads otherwise.
  bool proxy_ignored = false;
};
// Settings that must be off: every way to move a file, a clipboard, a device or sound across
// the connection, RemoteApp and remote assistance.
extern const char* const kForbiddenSettings[];
extern const int kForbiddenSettingCount;
// Settings that must be on: TLS, the graphics pipeline and its H.264 modes.
extern const char* const kRequiredSettings[];
extern const int kRequiredSettingCount;
// Settings that must be off for the connection to be what the design says: NLA (off in the
// image), the old RDP security layer, UDP, a Remote Desktop gateway, and FreeRDP's own
// reconnect (the bridge reconnects itself).
extern const char* const kOffSettings[];
extern const int kOffSettingCount;
// The only channels allowed: the graphics pipeline, the dynamic channel transport and input.
extern const char* const kAllowedChannels[];
extern const int kAllowedChannelCount;
// "" when the view is allowed, else what is wrong. A setting this check needs and the view
// lacks is wrong too (a FreeRDP that renamed it must not pass unchecked).
std::string settings_violation(const RdpSettingsView& v);

// ---- POST /scene

// What the bridge reads of a scene: its clock (epoch_unix_ms, required) and the direct-stream
// keys (admit_keys, at most 4, 64 hex digits each), checked as the phone streamer checks them;
// the rest of the body is the wall's and is ignored. The epoch is an integer of at most 9e15 in
// magnitude, the renderer's rule (scene.cpp), so the slot arithmetic stays in range.
struct BridgeScene {
  double epoch_unix_ms = 0;
  std::vector<std::string> admit_keys;  // raw (admit.h)
};
bool bridge_scene_parse(const std::string& body, BridgeScene* out, std::string* err);

// ---- the stream

// The renderer's frame rate per quality, for the delivered picture (renderer nvenc.cpp): bits
// per pixel at `fps`, between a floor and a cap. ENC_BPP overrides std's 0.8 as there.
uint32_t quality_rate_bps(int q, int w, int h, int fps);
// x264's constant rate factor per quality, under that rate as a VBV cap.
float quality_crf(int q);
// The header that opens a stream: the whole desktop, an image of its own.
std::string bridge_header_json(int w, int h, const std::string& codec, int fps, const char* q, uint32_t bitrate);
// A slot's bound: frame_slot clamps to +-2^53, where a double still counts by one and the
// encoder's slot + 1 cannot overflow.
#define FRAME_SLOT_LIMIT (1LL << 53)
// A frame's `t`: the 1/fps slot of its host time on the scene's clock (seconds since the last
// scene's epoch_unix_ms; 0 before any scene), as the phone streamer stamps its screen frames.
double frame_slot_t(double unix_ms, double epoch_unix_ms, int fps);
long long frame_slot(double unix_ms, double epoch_unix_ms, int fps);

// ---- /health

// An rdpgfx surface command's codec (MS-RDPEGFX 2.2.1.1, RDPGFX_CODECID_*), "0x00NN" for others.
std::string gfx_codec_name(uint32_t id);

enum BridgeState { STATE_CONNECTING, STATE_CONNECTED, STATE_DESKTOP, STATE_FAILED };
const char* bridge_state_name(BridgeState s);

// The host times (unix ms, -1 until it happens) of the steps to the desktop. All are the first
// since the process started, except connect_started: the start of the attempt that first
// reached TCP, or of the latest attempt before one did.
struct WindowsTimes {
  double start = -1, connect_started = -1, tcp = -1, rdp = -1, first_frame = -1, desktop = -1;
};

// The last durations of something, for percentiles.
class Durations {
 public:
  void add(double ms);
  double pct(double p) const;  // -1 without samples
  size_t n() const { return v_.size(); }

 private:
  std::deque<double> v_;
};

struct BridgeHealth {
  double fps = 0;
  int surface_w = 0, surface_h = 0;
  std::vector<std::pair<int, int>> streams;  // each stream's h and q
  double t = 0;
  uint32_t frame = 0;
  std::string boot_id;
  BridgeState state = STATE_CONNECTING;
  std::string error;       // why it failed (state failed), else ""
  std::string last_error;  // the last attempt's failure, kept while it retries
  uint64_t connects = 0, attempts = 0;
  WindowsTimes times;
  const char* desktop_by = "settled";
  double encode_p50 = -1, encode_p90 = -1;  // ms, -1: none yet
  double frame_age_ms = -1;                 // now minus the newest sent frame's stamp, -1: none
  double first_frame_sent_ms = -1;          // since start
  bool h264 = false;                        // FreeRDP was built with H.264
  // frames from rdpgfx EndFrames and from GDI EndPaints outside the pipeline, and the
  // pipeline's surface commands by codec (gfx_codec_name), as a JSON object
  uint64_t gfx_frames = 0, paint_frames = 0;
  std::string gfx_codecs = "{}";
  std::string viewers = "[]";
};
std::string bridge_health_json(const BridgeHealth& h);
