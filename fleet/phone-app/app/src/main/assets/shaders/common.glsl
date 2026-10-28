// Shared by every program: the frame's uniforms (FrameState), the palette, the integer hash.
#define PI 3.14159265
#define TAU 6.2831853
#define PERIOD 1200.0
#define LOOP_X 260.0
#define CLOUD_LOOP 900.0

uniform mat4 u_viewProj;
uniform mat4 u_invViewProj;
uniform mat4 u_lightViewProj;
uniform vec3 u_eye;
uniform vec3 u_camRight;
uniform vec3 u_camUp;
uniform float u_tw;        // scene time mod PERIOD
uniform float u_walkT;     // walking time mod PERIOD; stands still under the scene's moves
uniform float u_dance;     // 0..1
uniform float u_wave;      // 0..1
uniform float u_still;     // 0..1: the scene's moves stop the crowd (the phone's own moves do not)
uniform float u_beat;      // beats since the dance started
uniform float u_danceAge;  // seconds since the dance started
uniform float u_night;     // 0 day, 1 night
uniform vec2 u_spread;     // visible x range of the lawn
uniform vec3 u_palette[16];
uniform vec3 u_sunDir;
uniform vec3 u_sunCol;
uniform vec3 u_skyCol;
uniform vec3 u_groundCol;
uniform vec3 u_fogCol;
uniform vec3 u_horizonCol;
uniform vec3 u_zenithCol;
uniform vec2 u_vpSize;     // the whole image in pixels (the framebuffer's size)
uniform int u_lightCount;
uniform vec3 u_lightPos[16];
uniform vec3 u_lightCol[16];

uint hashu(uint x) {
    x ^= x >> 16; x *= 0x7feb352du;
    x ^= x >> 15; x *= 0x846ca68bu;
    x ^= x >> 16;
    return x;
}
float hash1(uint a) { return float(hashu(a) >> 8) * (1.0 / 16777216.0); }
float hash2(uint a, uint b) { return hash1(a ^ hashu(b + 0x9e3779b9u)); }
float wrapX(float x) { return x - LOOP_X * floor(x / LOOP_X + 0.5); }
vec3 palette(float i) { return u_palette[int(i + 0.5)]; }

vec3 rotX(vec3 p, float a) { float c = cos(a), s = sin(a); return vec3(p.x, c * p.y - s * p.z, s * p.y + c * p.z); }
vec3 rotY(vec3 p, float a) { float c = cos(a), s = sin(a); return vec3(c * p.x + s * p.z, p.y, -s * p.x + c * p.z); }
vec3 rotZ(vec3 p, float a) { float c = cos(a), s = sin(a); return vec3(c * p.x - s * p.y, s * p.x + c * p.y, p.z); }

// Filmic curve (ACES fit) and display gamma: linear light in, display values out.
vec3 tone(vec3 c) {
    c = max(c, vec3(0.0));
    c = (c * (2.51 * c + 0.03)) / (c * (2.43 * c + 0.59) + 0.14);
    return pow(clamp(c, 0.0, 1.0), vec3(1.0 / 2.2));
}

